use std::{collections::hash_map::Entry, fs, sync::Arc};

use anyhow::Context;
use camino::Utf8PathBuf;
use tracing::{error, info, info_span};
use utils::{fs_ext, id::TimelineId, lsn::Lsn};

use crate::{context::RequestContext, import_datadir, tenant::Tenant};

use super::Timeline;

/// A timeline with some of its files on disk, being initialized.
/// This struct ensures the atomicity of the timeline init: it's either properly created and inserted into pageserver's memory, or
/// its local files are removed. In the worst case of a crash, an uninit mark file is left behind, which causes the directory
/// to be removed on next restart.
///
/// The caller is responsible for proper timeline data filling before the final init.
#[must_use]
pub(crate) struct UninitializedTimeline<'t> {
    pub(crate) owning_tenant: &'t Tenant,
    timeline_id: TimelineId,
    raw_timeline: Option<Arc<Timeline>>,
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum TimelineCreationError {
    #[error("Already exists")]
    AlreadyExists,
    #[error("Already creating")]
    AlreadyCreating,
}

impl<'t> UninitializedTimeline<'t> {
    pub(crate) fn new(
        owning_tenant: &'t Tenant,
        timeline_id: TimelineId,
        raw_timeline: Option<Arc<Timeline>>,
    ) -> Result<Self, TimelineCreationError> {
        let timelines = owning_tenant.timelines.lock().unwrap();
        let mut creating_timelines = owning_tenant.timelines_creating.lock().unwrap();

        if timelines.contains_key(&timeline_id) {
            Err(TimelineCreationError::AlreadyExists)
        } else if creating_timelines.contains(&timeline_id) {
            Err(TimelineCreationError::AlreadyCreating)
        } else {
            creating_timelines.insert(timeline_id);
            Ok(Self {
                owning_tenant,
                timeline_id,
                raw_timeline,
            })
        }
    }

    /// Finish timeline creation: insert it into the Tenant's timelines map
    ///
    /// This function launches the flush loop if not already done.
    ///
    /// The caller is responsible for activating the timeline (function `.activate()`).
    pub(crate) fn finish_creation(mut self) -> anyhow::Result<Arc<Timeline>> {
        let timeline_id = self.timeline_id;
        let tenant_id = self.owning_tenant.tenant_id;

        let new_timeline = self.raw_timeline.take().with_context(|| {
            format!("No timeline for initalization found for {tenant_id}/{timeline_id}")
        })?;

        // Check that the caller initialized disk_consistent_lsn
        let new_disk_consistent_lsn = new_timeline.get_disk_consistent_lsn();
        anyhow::ensure!(
            new_disk_consistent_lsn.is_valid(),
            "new timeline {tenant_id}/{timeline_id} has invalid disk_consistent_lsn"
        );

        let mut timelines = self.owning_tenant.timelines.lock().unwrap();
        match timelines.entry(timeline_id) {
            Entry::Occupied(_) => anyhow::bail!(
                "Found freshly initialized timeline {tenant_id}/{timeline_id} in the tenant map"
            ),
            Entry::Vacant(v) => {
                v.insert(Arc::clone(&new_timeline));

                new_timeline.maybe_spawn_flush_loop();
            }
        }

        Ok(new_timeline)
    }

    /// Prepares timeline data by loading it from the basebackup archive.
    pub(crate) async fn import_basebackup_from_tar(
        self,
        copyin_read: &mut (impl tokio::io::AsyncRead + Send + Sync + Unpin),
        base_lsn: Lsn,
        broker_client: storage_broker::BrokerClientChannel,
        ctx: &RequestContext,
    ) -> anyhow::Result<Arc<Timeline>> {
        let raw_timeline = self.raw_timeline()?;

        import_datadir::import_basebackup_from_tar(raw_timeline, copyin_read, base_lsn, ctx)
            .await
            .context("Failed to import basebackup")?;

        // Flush the new layer files to disk, before we make the timeline as available to
        // the outside world.
        //
        // Flush loop needs to be spawned in order to be able to flush.
        raw_timeline.maybe_spawn_flush_loop();

        fail::fail_point!("before-checkpoint-new-timeline", |_| {
            anyhow::bail!("failpoint before-checkpoint-new-timeline");
        });

        raw_timeline
            .freeze_and_flush()
            .await
            .context("Failed to flush after basebackup import")?;

        // All the data has been imported. Insert the Timeline into the tenant's timelines
        // map and remove the uninit mark file.
        let tl = self.finish_creation()?;
        tl.activate(broker_client, None, ctx);
        Ok(tl)
    }

    pub(crate) fn raw_timeline(&self) -> anyhow::Result<&Arc<Timeline>> {
        self.raw_timeline.as_ref().ok_or(anyhow::anyhow!(
            "No raw timeline {}/{} found",
            self.owning_tenant.tenant_id,
            self.timeline_id
        ))
    }
}

impl Drop for UninitializedTimeline<'_> {
    fn drop(&mut self) {
        if let Some(timeline) = self.raw_timeline.take() {
            let _entered = info_span!("drop_uninitialized_timeline", tenant_id = %self.owning_tenant.tenant_id, timeline_id = %self.timeline_id).entered();
            error!("Timeline got dropped without initializing, cleaning its files");
            cleanup_timeline_directory(&timeline.get_path());
        }

        // If we succeeded, the timeline is now in [`Tenant::timelines`] and this takes over
        // to block colliding creations in [`UninitializedTimeline::new`].
        self.owning_tenant
            .timelines_creating
            .lock()
            .unwrap()
            .remove(&self.timeline_id);
    }
}

pub(crate) fn cleanup_timeline_directory(timeline_path: &Utf8PathBuf) {
    match fs_ext::ignore_absent_files(|| fs::remove_dir_all(timeline_path)) {
        Ok(()) => {
            info!("Timeline dir {timeline_path:?} removed successfully, removing the uninit mark")
        }
        Err(e) => {
            error!("Failed to clean up uninitialized timeline directory {timeline_path:?}: {e:?}")
        }
    }
}

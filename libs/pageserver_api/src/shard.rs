use std::hash::Hasher;

use crate::key::Key;
use mur3;
use utils::id::NodeId;

#[derive(Ord, PartialOrd, Eq, PartialEq, Clone, Copy)]
struct ShardNumber(u8);

#[derive(Ord, PartialOrd, Eq, PartialEq, Clone, Copy)]
struct ShardCount(u8);

impl ShardNumber {
    fn within_count(&self, rhs: ShardCount) -> bool {
        self.0 < rhs.0
    }
}

/// Stripe size in number of pages
#[derive(Clone, Copy)]
struct ShardStripeSize(u32);

/// Layout version: for future upgrades where we might change how the key->shard mapping works
#[derive(Clone, Copy)]
struct ShardLayout(u8);

const LAYOUT_V1: ShardLayout = ShardLayout(1);

/// Default stripe size in pages: 256MiB divided by 8kiB page size.
const DEFAULT_STRIPE_SIZE: ShardStripeSize = ShardStripeSize(256 * 1024 / 8);

/// The ShardIdentity contains the information needed for one member of map
/// to resolve a key to a shard, and then check whether that shard is ==self.
#[derive(Clone, Copy)]
struct ShardIdentity {
    layout: ShardLayout,
    number: ShardNumber,
    count: ShardCount,
    stripe_size: ShardStripeSize,
}

/// The location of a shard contains both the logical identity of the pageserver
/// holding it (control plane's perspective), and the physical page service port
/// that postgres should use (endpoint's perspective).
#[derive(Clone)]
struct ShardLocation {
    id: NodeId,
    page_service: (url::Host, u16),
}

/// The ShardMap is sufficient information to map any Key to the page service
/// which should store it.
#[derive(Clone)]
struct ShardMap {
    layout: ShardLayout,
    count: ShardCount,
    stripe_size: ShardStripeSize,
    pageservers: Vec<Option<ShardLocation>>,
}

impl ShardMap {
    pub fn get_location(&self, shard_number: ShardNumber) -> &Option<ShardLocation> {
        assert!(shard_number.within_count(self.count));
        self.pageservers.get(shard_number.0 as usize).unwrap()
    }

    pub fn get_identity(&self, shard_number: ShardNumber) -> ShardIdentity {
        assert!(shard_number.within_count(self.count));
        ShardIdentity {
            layout: self.layout,
            number: shard_number,
            count: self.count,
            stripe_size: self.stripe_size,
        }
    }

    pub fn get_shard_number(&self, key: &Key) -> ShardNumber {
        key_to_shard_number(self.count, self.stripe_size, key)
    }

    pub fn default_with_shards(shard_count: ShardCount) -> Self {
        ShardMap {
            layout: LAYOUT_V1,
            count: shard_count,
            stripe_size: DEFAULT_STRIPE_SIZE,
            pageservers: (0..shard_count.0 as usize).map(|_| None).collect(),
        }
    }
}

impl ShardIdentity {
    pub fn get_shard_number(&self, key: &Key) -> ShardNumber {
        key_to_shard_number(self.count, self.stripe_size, key)
    }
}

impl Default for ShardIdentity {
    /// The default identity is to be the only shard for a tenant, i.e. the legacy
    /// pre-sharding case.
    fn default() -> Self {
        ShardIdentity {
            layout: LAYOUT_V1,
            number: ShardNumber(0),
            count: ShardCount(1),
            stripe_size: DEFAULT_STRIPE_SIZE,
        }
    }
}

/// Where a Key is to be distributed across shards, select the shard.  This function
/// does not account for keys that should be broadcast across shards.
fn key_to_shard_number(count: ShardCount, stripe_size: ShardStripeSize, key: &Key) -> ShardNumber {
    // Fast path for un-sharded tenants
    if count == ShardCount(0) {
        return ShardNumber(0);
    }

    let mut hasher = mur3::Hasher32::with_seed(0);
    hasher.write_u8(key.field1);
    hasher.write_u32(key.field2);
    hasher.write_u32(key.field3);
    hasher.write_u32(key.field4);
    let hash = hasher.finish32();

    let blkno = key.field6;

    let stripe = hash + (blkno / stripe_size.0);

    let shard = stripe as u8 % (count.0 as u8);

    ShardNumber(shard)
}

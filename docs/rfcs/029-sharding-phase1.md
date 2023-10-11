# Sharding Phase 1: Static Key-space Sharding

## Summary

To enable databases with sizes approaching the capacity of a pageserver's disk,
it is necessary to break up the storage for the database, or _shard_ it.

Sharding in general is a complex area.  This RFC aims to define an initial
capability that will permit creating large-capacity databases using a static configuration
defined at time of Tenant creation.

## Motivation

Currently, all data for a Tenant, including all its timelines, is stored on a single
pageserver.  The local storage required may be several times larger than the actual
database size, due to LSM write inflation.

If a database is larger than what one pageserver can hold, then it becomes impossible
for the pageserver to hold it in local storage, as it must do to provide service to
clients.

### Prior art

In Neon:
- Layer File Spreading: https://www.notion.so/neondatabase/One-Pager-Layer-File-Spreading-Konstantin-21fd9b11b618475da5f39c61dd8ab7a4
- Layer File SPreading: https://www.notion.so/neondatabase/One-Pager-Layer-File-Spreading-Christian-eb6b64182a214e11b3fceceee688d843
- Key Space partitioning: https://www.notion.so/neondatabase/One-Pager-Key-Space-Partitioning-Stas-8e3a28a600a04a25a68523f42a170677

Prior art in other distributed systems is too broad to capture here: pretty much
any scale out storage system does something like this.

## Requirements

- Enable creating a large (for example, 16TiB) database without requiring dedicated
  pageserver nodes.
- Share read/write bandwidth costs for large databases across pageservers, as well
  as storage capacity, in order to avoid large capacity databases acting as I/O hotspots
  that disrupt service to other tenants.
- Our data distribution scheme should handle sparse/nonuniform keys well, since postgres
  does not write out a single contiguous ranges of page numbers.

*Note: the definition of 'large database' is arbitrary, but the lower bound is to ensure that a database
that a user might create on a current-gen enterprise SSD should also work well on
Neon.  The upper bound is whatever postgres can handle: i.e. we must make sure that the
pageserver backend is not the limiting factor in the database size*.

## Non Goals

- Independently distributing timelines within the same tenant.  If a tenant has many
  timelines, then sharding may be a less efficient mechanism for distributing load than
  sharing out timelines between pageservers.
- Distributing work in the LSN dimension: this RFC focuses on the Key dimension only,
  based on the idea that separate mechanisms will make sense for each dimension.

## Impacted Components

pageserver, control plane, postgres/smgr

## Terminology

**Key**: a postgres page number, qualified by relation.  In the sense that the pageserver is a versioned key-value store,
the page number is the key in that store.  `Key` is a literal data type in existing code.

**LSN dimension**: this just means the range of LSNs (history), when talking about the range
of keys and LSNs as a two dimensional space.

## Implementation

### Key sharding vs. LSN sharding

When we think of sharding across the two dimensional key/lsn space, this is an
opportunity to think about how the two dimensions differ:
- Sharding the key space distributes the _write_ workload of ingesting data
  and compacting.  This work must be carefully managed so that exactly one
  node owns a given key.
- Sharding the LSN space distributes the _historical read_ workload.  This work
  can be done by anyone without any special coordination, as long as they can
  see the remote index and layers.

The key sharding is the harder part, and also the more urgent one, to support larger
capacity databases.  Because distributing historical LSN read work is a relatively
simpler problem that most users don't have, we defer it to future work.  It is anticipated
that some quite simple P2P offload model will enable distributing work for historical
reads: a node which is low on space can call out to peer to ask it to download and
serve reads from a historical layer.

### Key mapping scheme

Having decided to focus on key sharding, we must next decide how we will map
keys to shards.  It is proposed to use a "wide striping" approach, to obtain a good compromise
between data locality and avoiding entire large relations mapping to the same shard.

We will define three spaces:
- Key space: unsigned integer
- Stripe space: unsigned integer
- Shard space: integer from 0 to N-1, where we have N shards.

### Stripe -> Shard mapping

The main property want want from our stripe->shard mapping is
to spread bandwidth when high throughput contiguous writes are
happening, such as appending to a relation.

Stripes are mapped to shards with a simple modulo operation:

```
Stripe | 00 | 01 | 02 | 03 | 04 | 05 | 06 | 07 | 08 | 09 | ...
Shard  | 00 | 01 | 02 | 01 | 02 | 03 | 01 | 02 | 03 | 01 | ...
```

### Key -> Stripe mapping

Keys are currently defined in the pageserver's getpage@lsn interface as follows:
```
pub struct Key {
    pub field1: u8,
    pub field2: u32,
    pub field3: u32,
    pub field4: u32,
    pub field5: u8,
    pub field6: u32,
}


fn rel_block_to_key(rel: RelTag, blknum: BlockNumber) -> Key {
    Key {
        field1: 0x00,
        field2: rel.spcnode,
        field3: rel.dbnode,
        field4: rel.relnode,
        field5: rel.forknum,
        field6: blknum,
    }
}
```

_Note: keys for relation metadata are ignored here, as this data will be mirrored to all
shards.  For distribution purposes, we only care about user data keys_

The properties we want from our Key->Stripe mapping are:
- Locality in `blknum`, such that adjacent `blknum` will usually map to
  the same stripe and consequently land on the same shard, even though the overall
  collection of blocks in a relation will be spread over many stripes and therefore
  many shards. 
- Avoid the same blknum on different relations landing on the same stripe, so that
  with many small relations we do not end up aliasing data to the same stripe/shard.
- Avoid vulnerability to aliasing in the values of relation identity fields, such that
  if there are patterns in the value of `relnode`, these do not manifest as patterns
  in data placement.

To achieve these goals, we may use a hybrid approach where the relation part of the
key is hashed, and the `blknum` part is used literally.  To map a `Key` to a stripe:
- Hash the `Key` fields 1-4, to select a stripe offset.
- Divide field 6 (`blknum`) field by the stripe size in pages, and add this to the stripe
  number in the previous step.

We ignore `forknum` for key mapping, because it distinguishes different classes of data
in the same relation, and we would like to keep the data in a relation together.

Hashing fields 1-4 could be avoided if we chose to make some assumptions about how postgres
picks values for `relnode`: if we assumed these values were reasonably well
distributed, then we could skip the hashing.  However, this is a dangerous assumption:
even if postgres itself assigns relnodes sequentially, a user workload could do something like
creating 50 pairs of 2 tables, and then deleting the first table in each pair: the resulting
sequentially allocated relnodes would then have a pattern that would lead to aliasing in
shard placement.

### Data placement examples

For example, consider the extreme large databases cases of postgres data layout in a system with 8 shards
and a stripe size of 32k pages:
- A single large relation: `blknum` division will break the data up into 4096
  stripes, which will be assigned round-robin to the shards, wrapping many times
  as the number of stripes is much greater than the number of shards.
- 4096 relations of of 32k pages each: each relation will map to exactly one stripe,
  and that stripe will be placed according to the hash of the key fields 1-5.  The
  data placement will be statistically uniform across shards.

Data placement will be much less even on smaller databases:
- A tenant with 2 shards and 2 relations of one stripe size each: there is a 50% chance
  that both relations land on the same shard and no data lands on the other shard.
- A tenant with 8 shards and one relation of size 12 stripes: 4 shards will have double
  the data of the other four shards.

These uneven cases for small amounts of data do not matter, as long as the stripe size
is an order of magnitude smaller than the amount of data we are comfortable holding
in a single shard: if our system handles shard sizes up to 10-100GB, then it is not an issue if
a tenant has some shards with 256MB size and some shards with 512MB size, even though
the standard deviation of shard size within the tenant is very high.  Our key mapping
scheme provides a statistical guarantee that as the tenant's overall data size increases, 
uniformity of placement will improve.

### Important Types

#### `ShardMap`

Provides all the information needed to route a request for a particular
key to the correct pageserver:
- Layout version: this is initially 1, and can be incremented in future if we
  choose to define alternative key mapping schemes.
- Stripe size
- Shard count
- Address of the pageserver hosting each shard

This structure's size is linear with the number of shards.

#### `ShardIdentity`

Provides the information needed to know whether a particular key belongs
to a particular shard:
- Layout version
- Stripe size
- Shard count
- Shard index

This structure's size is constant.

### Pageserver changes

#### Structural

Everywhere the Pageserver currently deals with Tenants, it will move to dealing with
`TenantShard`s, which are just a `Tenant` plus a `ShardIdentity` telling it which part
of the keyspace it owns.  An un-sharded tenant is just a `TenantShard` whose `ShardIdentity`
covers the whole keyspace.

When the pageserver writes layers and index_part.json to remote storage, it must
include the shard index & count in the name, to avoid collisions (the count is
necessary for future-proofing: the count will vary in time).  These keys
will also include a generation number: the [generation numbers](025-generation-numbers.md) system will work
exactly the same for TenantShards as it does for Tenants today: each shard will have
its own generation number.

#### WAL Ingest

The 0th shard in a tenant will be the only one that subscribes to the safekeeper to receive
WAL updates.  It will do decoding via walredo the same way as the current code, but then
instead of applying all the resulting deltas to its local Timeline, these will be scattered
out according to the key of updated pages.

The pageserver will expose a new API for peer pageservers to send such delta
writes to timelines.  The `ShardMap` will be provided to the pageserver.  Only the 0th shard
needs the `ShardMap`, but for simplicity we can supply it to all TenantShards within
a tenant.  The control plane is responsible for sending updates to the pageserver
when the `ShardMap` changes.

The 0th shard will issue writes to many peers concurrently, but impose a bound on how
far the WAL will be consumed while waiting for writes to drain to peers -- if one peer
is not servicing write requests, it will block the overall consumption of the WAL.

Publishing updates to `remote_consistent_lsn` to safekeepers on the 0th shard
will be based on feedback from peers: if a peer has writes pending (e.g. lsn consumed
but remote_consistent_lsn not yet advanced), then the safekeeper-visible `remote_consistent_lsn`
may not be advanced past that peer's `remote_consistent_lsn`.

#### Compaction/GC

No changes needed.

The pageserver doesn't have to do anything special during compaction
or GC.  It is implicitly operating on the subset of keys that map to its ShardIdentity.
This will result in sparse layer files, containing keys only in the stripes that this
shard owns.  Where optimizations currently exist in compaction for spotting "gaps" in
the key range, these should be updated to ignore gaps that are due to sharding, to
avoid spuriously splitting up layers ito stripe-sized pieces.

### Compute Endpoints

Compute endpoints will need to:
- Accept a ShardMap as part of their configuration from the control plane
- Route pageserver requests according to that ShardMap

This will not be trivial, but is necessary to enable sharding tenants without
adding latency from extra hops through some intermediate service.

### Control Plane

Tenants, or _Projects_ in the control plane, will each own a set of TenantShards (this will
be 1 for small tenants).  Logic for placement of tenant shards is just the same as the current logic for placing
tenants.

Tenant lifecycle operations like deletion will require fanning-out to all the shards
in the tenant.  The same goes for timeline creation and deletion: a timeline should
not be considered created until it has been created in all shards.

#### Publishing ShardMap updates

The control plane is the source of truth for the `ShardMap`, since it is the coordinating
entitity that knows about all the shards and which pageservers they are attached to.  This
structure is then provided to the pageserver (for fanning out ingested WAL), and to the
compute endpoints to enable them to find the right pageserver for a particular page.

#### Selectively enabling sharding for large tenants

The control plane will enable setting a "large tenant" hint during tenant creation via some UI,
and use this to define the number of shards to create.  The UI
for setting this hint doesn't have to be generally visible to users: it may be
something that is done by administrators for onboarding special known-large workloads.

In future, this hint mechanism will become optional when we implement automatic
re-sharding of tenants.

## Future Work

Clearly, the mechanism described in this RFC has substantial limitations:
1. the number of shards in a tenant is defined at creation time.
2. data is not distributed across the LSN dimension
3. the work of fanning-out the WAL is done by the 0th shard pageserver

### Splitting

To address `A`, a _splitting_ feature will later be added.  One shard can split its
data into a number of children by doing a special compaction operation to generate
image layers broken up child-shard-wise, and then writing out an index_part.json for
each child.  This will then require external coordination (by the control plane) to
safely attach these new child shards and then move them around to distribute work.
The opposite _merging_ operation can also be imagined, but is unlikely to be implemented:
once a Tenant has been sharded, the marginal efficiency benefit of merging is unlikely to justify
the risk/complexity of implementing such a rarely-encountered scenario.

### Distributing work in the LSN dimension

To address `B`, it is envisaged to have some gossip mechanism for pageservers to communicate
about their workload, and then a getpageatlsn offload mechanism where one pageserver can
ask another to go read the necessary layers from remote storage to serve the read.  This
requires relativly little coordination because it is read-only: any node can service any
read.  All reads to a particular shard would still flow through one node, but the
disk capactity & I/O impact of servicing the read would be distributed.

### Relieving pageserver of WAL-ingestion work

The business of decoding the WAL stream and sending page deltas to the relevant pageservers
is stateless and does not need to be part of the pageserver.

Moving this into a separate service would be advantageous:
- avoid a CPU/network "hot spot" on the 0th shard when dealing with a tenant doing
  lots of writes.
- we may use non-storage EC2 instances to do the fan-out work, which are cheaper per-cpu-core
- we may scale the CPU/network resources for ingestion independently of how we scale
  pageservers for capacity

## FAQ/Alternatives

### Why stripe the data, rather than using contiguous ranges of keyspace for each shard?

When a database is growing under a write workload, writes may predominantly hit the
end of the keyspace, creating a bandwidth hotspot on that shard.  Similarly, if the user
is intensively re-writing a particular relation, if that relation lived in a particular
shard then it would not achieve our goal of distributing the write work across shards.

### Why not proxy read requests through one pageserver, so that endpoints don't have to change?

1. This would not achieve scale-out of network bandwidth: a busy tenant with a large
   database would still cause a load hotspot on the pageserver routing its read requests. 
2. The additional hop through the "proxy" pageserver would add latency and overall
   resource cost (CPU, network bandwidth)

### Layer File Spreading: use one pageserver as the owner of a tenant, and have it spread out work on a per-layer basis to peers

In this model, there would be no explicit sharding of work, but the pageserver to which
a tenant is attached would not hold all layers on its disk: instead, it would call out
to peers to have them store some layers, and call out to those peers to request reads
in those layers.

This mechanism will work well for distributing work in the LSN dimension, but in the key
space dimension it has the major limitation of requiring one node to handle all
incoming writes, and compactions.  Even if the write workload for a large database 
fits in one pageserver, it will still be a hotspot and such tenants may still
de-facto require their own pageserver.

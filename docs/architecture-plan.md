# cownfs: architecture and implementation plan

**Status:** initial design, Rust selected as the primary implementation language  
**Scope:** single-node, single-device, userspace copy-on-write filesystem exported only through NFSv4.0

## 1. Design in one page

`cownfs` is a storage engine and NFS server in one process. It never exposes a local mount API and has no FUSE, NFS-Ganesha, or FSAL dependency. The storage engine uses immutable 4 KiB blocks, copy-on-write B-trees, checksummed parent pointers, and two ping-pong superblocks. A transaction becomes visible only when a valid superblock with the next generation is durable. Snapshots are additional references to existing tree roots; subsequent writes copy only the diverging paths.

The first implementation is deliberately narrow: one process, one backing file or block device, one writer transaction at a time, NFSv4.0 over TCP/2049, and AUTH_SYS. Linux is the first server platform. Client portability comes from NFS: Linux and macOS use their native NFS clients.

```text
     Linux client          macOS client          test client
   (kernel NFSv4.0)      (kernel NFSv4.0)          (pynfs)
          |                     |                     |
          +---------------------+---------------------+
                                |
                     TCP :2049 / ONC RPC / XDR
                                |
                  +-------------v--------------+
                  | NFSv4.0 COMPOUND frontend  |
                  | auth, filehandles, state,   |
                  | leases, opens, locks        |
                  +-------------+--------------+
                                |
                  +-------------v--------------+
                  | Engine API (VFS-like)       |
                  | inode/dir/extent operations |
                  | snapshot operations         |
                  +-------------+--------------+
                                |
                  +-------------v--------------+
                  | Transaction manager         |
                  | dirty set, CoW, commit,     |
                  | root/refcount transitions   |
                  +-------------+--------------+
                                |
                  +-------------v--------------+
                  | Block store + allocator     |
                  | 4 KiB I/O, checksums,       |
                  | free-space bitmap           |
                  +-------------+--------------+
                                |
                     backing file or block device
```

---

## 2. Goals and non-goals

### Goals

- **Crash consistency without a journal:** pure CoW updates plus an atomic, checksummed superblock commit.
- **Bit-rot detection:** verify metadata through checksums embedded in parent pointers and verify file data against extent checksums.
- **Cheap snapshots:** create a snapshot by adding root references; copy only modified paths and data thereafter.
- **Portable service boundary:** keep the server in userspace and expose only NFSv4.0. Start on Linux; clients need only their native NFS implementation.
- **Recoverability:** provide offline inspection and `fsck` from the first phase, including mark-and-sweep reclamation of blocks leaked by interrupted transactions.

### Non-goals for v1

- Kerberos or RPCSEC_GSS; v1 accepts AUTH_SYS only.
- NFSv4.1 sessions, pNFS, and NFSv4.2 operations.
- Delegations, named attributes, or ACLs beyond Unix mode bits.
- Multiple devices, RAID, erasure coding, clustering, or high availability.
- A local mount path, FUSE frontend, Ganesha integration, or FSAL plugin.
- Online repair of corrupted data. On a single device, checksums can detect corruption but cannot reconstruct a lost block.

---

## 3. Layered architecture

### 3.1 Block layer

The block layer owns a file descriptor and exposes exact 4 KiB reads and writes. It must use `pread`/`pwrite`-style positional I/O, reject short I/O, validate block-number bounds, and make durability explicit through `fdatasync` or `fsync`. The same interface serves a regular image and a raw block device.

Responsibilities:

- Read and write numbered 4 KiB blocks.
- Reserve blocks 0 and 1 for superblocks.
- Allocate and free block runs through a free-space bitmap.
- Treat the bitmap as transactional metadata: update it by CoW and publish its new root only at commit.
- Keep the allocator root outside snapshots. A snapshot preserves filesystem-tree roots, not historical free-space state.
- Provide fault-injection hooks around allocation, writes, flushes, and superblock publication.

Recommended interface:

```rust
struct BlockNo(u64);
struct Block([u8; 4096]);

trait BlockStore {
    fn read(&self, block: BlockNo) -> Result<Block>;
    fn write(&self, block: BlockNo, data: &Block) -> Result<()>;
    fn sync_data(&self) -> Result<()>;
    fn block_count(&self) -> u64;
}
```

The allocator must never hand out a block reachable from the active generation or any snapshot. A crash can leak newly allocated but unpublished blocks; it must not make a published block appear free. `fsck --reclaim` repairs leaks by walking every live root and rebuilding the bitmap.

### 3.2 Transaction layer

Use one writer transaction at a time for v1. Readers pin an immutable committed root set and never observe a half-committed graph. A write transaction owns:

- its base generation;
- new roots for each modified tree;
- newly allocated blocks;
- deferred decrements and recursive frees;
- a dirty-block map;
- buffered NFS `UNSTABLE` writes.

Commit sequence:

1. Freeze the transaction and reject further mutation into it.
2. Materialize new data blocks, B-tree nodes, refcount changes, and the CoW free-space bitmap.
3. Write all dirty non-superblock blocks.
4. Call `fsync`/`fdatasync` so those writes reach stable storage.
5. Serialize generation `N+1` into the inactive superblock slot, including all new roots and a self-checksum.
6. Write that complete 4 KiB superblock and sync again.
7. Publish the new generation in memory. There is no separate on-disk active-bit write: the valid slot with the highest generation is active after restart.

If any step before 6 fails, generation `N` remains authoritative. A torn superblock fails its checksum, so mount falls back to the other slot.

**NFS durability mapping is explicit:**

- `WRITE(stable=UNSTABLE)` may remain in memory and returns a write verifier; it does not force a filesystem transaction commit.
- `WRITE(stable=FILE_SYNC)` applies the write and commits the transaction before replying success.
- `COMMIT` commits all covered unstable writes before replying success.
- If the server restarts and loses unstable data, its write verifier must change so the client knows to resend.

For the initial implementation, `DATA_SYNC` may be treated as `FILE_SYNC`. Group concurrent stable requests into one commit only after correctness is established.

### 3.3 Engine API

The engine is independent of RPC and XDR. It accepts validated identities, opaque inode numbers, byte ranges, and names; it returns typed errors that the frontend maps to NFS status codes. Do not leak NFS stateids into the storage layer.

```rust
trait Engine {
    fn lookup(&self, view: &View, parent: Ino, name: &[u8]) -> Result<Inode>;
    fn getattr(&self, view: &View, ino: Ino) -> Result<Attrs>;
    fn setattr(&self, txn: &mut Txn, ino: Ino, patch: AttrPatch) -> Result<()>;
    fn read(&self, view: &View, ino: Ino, off: u64, len: u32) -> Result<ReadResult>;
    fn write(&self, txn: &mut Txn, ino: Ino, off: u64, data: &[u8]) -> Result<WriteResult>;

    fn create(&self, txn: &mut Txn, parent: Ino, name: &[u8], args: CreateArgs) -> Result<Inode>;
    fn mkdir(&self, txn: &mut Txn, parent: Ino, name: &[u8], mode: Mode) -> Result<Inode>;
    fn remove(&self, txn: &mut Txn, parent: Ino, name: &[u8]) -> Result<()>;
    fn rename(&self, txn: &mut Txn, old_parent: Ino, old_name: &[u8],
              new_parent: Ino, new_name: &[u8]) -> Result<()>;
    fn link(&self, txn: &mut Txn, target: Ino, parent: Ino, name: &[u8]) -> Result<()>;
    fn symlink(&self, txn: &mut Txn, parent: Ino, name: &[u8], target: &[u8]) -> Result<Inode>;
    fn readlink(&self, view: &View, ino: Ino) -> Result<Vec<u8>>;
    fn readdir(&self, view: &View, ino: Ino, cookie: Cookie, max_bytes: u32) -> Result<DirPage>;

    fn snapshot_create(&self, txn: &mut Txn, name: String) -> Result<SnapshotId>;
    fn snapshot_delete(&self, txn: &mut Txn, id: SnapshotId) -> Result<()>;
    fn snapshot_list(&self, view: &View) -> Result<Vec<Snapshot>>;
}
```

`View` pins either the current committed roots or a snapshot. `Txn` is the only mutation capability. Each mutating operation updates all related trees in one transaction: for example, create inserts the inode and directory entry together; rename updates both directories and link counts atomically.

### 3.4 NFSv4.0 frontend

The frontend implements ONC RPC record marking over TCP port 2049, minimal AUTH_NONE/AUTH_SYS decoding, XDR, and NFSv4.0 COMPOUND dispatch. It does not use `libtirpc`. Generate nothing dynamically at runtime; transcribe and test only the XDR types and operation arms needed from RFC 7530/7531. Expect roughly 2–3 KLOC of mechanical codec and dispatch code.

A connection parser must cap record fragments, compound operation counts, UTF-8/name lengths, attribute bitmap sizes, and opaque fields before allocation. Decode into bounded request objects; encode directly into a bounded response buffer. Unknown operations return the protocol-defined unsupported-operation status rather than closing the connection.

**v1 wire surface:** RPC `NULL`, and NFS `COMPOUND` with `PUTROOTFH`, `PUTFH`, `GETFH`, `SAVEFH`, `RESTOREFH`, `LOOKUP`, `LOOKUPP`, `READDIR`, `READ`, `WRITE`, `COMMIT`, `CREATE`, `REMOVE`, `RENAME`, `LINK`, `READLINK`, `GETATTR`, `SETATTR`, `ACCESS`, `SETCLIENTID`, `SETCLIENTID_CONFIRM`, `OPEN`, `CLOSE`, `LOCK`, `LOCKU`, `RENEW`, and `RELEASE_LOCKOWNER`. `SAVEFH` is required to express `RENAME` and `LINK` compounds.

Several requested filesystem actions are not distinct NFSv4.0 operations:

- Map the engine's `mkdir` to `CREATE` with object type `NF4DIR`, and symlink creation to `CREATE` with `NF4LNK`.
- `FSINFO`, `PATHCONF`, and `FSSTAT` are not NFSv4.0 procedures. Return their capacity, limit, and behavior information through bitmap-driven `GETATTR` attributes. Internal engine queries may retain those names.

`READDIR` is included because the P4 `ls -R` gate cannot work without it. Regular-file creation occurs through `OPEN` with `OPEN4_CREATE`, not `CREATE`.

---

## 4. On-disk format

All integers are fixed-width and encoded in one chosen byte order; use explicit load/store helpers rather than casting packed structs onto disk bytes. Every structure begins with a type/version field and reserves bytes for compatible growth.

### 4.1 Superblocks

Blocks 0 and 1 are ping-pong superblock slots. Each contains:

```text
magic, format_version, block_size, filesystem_uuid,
generation, next_inode_number,
inode_root, directory_root, extent_root,
snapshot_root, free_bitmap_root,
feature_flags, checksum_algorithm, self_checksum
```

Each root is a block pointer. Mount reads both slots, validates magic/version/size/self-checksum and root bounds, then selects the valid slot with the highest generation. Equal-generation disagreement is corruption. `mkfs` writes generation 1 to one slot and leaves the other invalid or at generation 0.

### 4.2 Block pointers and headers

A metadata block pointer is:

```text
{ block_number: u64, checksum: u64, generation: u64 }
```

The checksum algorithm may produce 32 or 64 bits; store it in a fixed 64-bit field and zero unused high bits. The checksum is held by the parent pointer and verified before interpreting the child. A root pointer in a superblock plays the same parent role.

Each B-tree block header contains at least:

```text
type, level, item_count, free_offset, generation, refcount: u32
```

The target refcount semantics are incoming references from parent nodes plus root references. Snapshot creation increments each referenced tree root. Copying a parent adds references to unchanged children. Before modifying a node whose refcount exceeds one, copy it, redirect the transaction's parent pointer, then decrement the old node. A zero count recursively decrements children and frees the block.

Refcount changes are part of the transaction's dirty metadata set and become authoritative only with the new superblock generation. The P1 model must verify every parent/root edge against the stored count after each commit and after simulated crashes; snapshot support does not proceed until this invariant holds.

### 4.3 Generic CoW B-tree

Implement one B+tree over variable-length byte-string keys and values. A tree descriptor supplies comparison and key validation. Internal nodes contain separator keys and checked child pointers; leaves contain sorted key/value items. Use a slotted-page layout: fixed-size item descriptors grow upward while key/value payloads grow downward. Split by occupied bytes, not item count.

The three global filesystem trees are:

| Tree | Key | Value |
|---|---|---|
| Inode | `u64 ino` | inode record |
| Directory | parent + name | child ino + type |
| Extent | ino + logical offset | physical run + checksum |

Composite integer fields are big-endian so lexicographic byte comparison preserves numeric order:

- **Inode tree:** key is the 64-bit inode number.
- **Directory tree:** key is `u64 parent_ino` in big-endian order followed by raw name bytes.
- **Extent tree:** key is `u64 ino || u64 logical_offset`, both big-endian. The value holds physical start block, block count, allocation generation, and data checksum.

The inode record is fixed at approximately 128 bytes: mode/type, UID, GID, link count, size, allocated bytes, atime, mtime, ctime, flags, and an inode generation counter. That counter increments whenever an inode number is reused and is part of stale-filehandle detection.

The initial inode allocator is a monotonically increasing counter in the superblock, advanced transactionally. Deletion need not recycle inode numbers in v1; if reuse is later enabled, generation must change before publication.

### 4.4 Extents and data checksums

Writes allocate new physical blocks even when replacing existing logical ranges. Partial-block writes read and verify the old block, merge bytes in memory, and write a complete new block. Only after new data is durable may the transaction publish the changed extent mapping.

Start with one-block extents to simplify split/overwrite correctness. Once stable, coalesce adjacent logical and physical blocks into runs. The checksum covers the referenced data; if a run stores one checksum, reading any part of it must verify the complete run. A later format revision may store one checksum per 4 KiB block for bounded verification cost.

Supported checksum choices are CRC32C and xxHash64. Both target accidental corruption detection, not adversarial integrity. Persist the algorithm identifier in the superblock; never infer it from checksum width.

### 4.5 Free space, snapshots, and recovery

The free-space bitmap is itself CoW-updated each transaction and rooted directly in the superblock. It is never included in a snapshot. Reserve newly allocated blocks in the transaction's private bitmap version; publish that bitmap together with all filesystem roots.

A snapshot record contains an ID, name, creation generation/time, and copies of the inode, directory, and extent roots. Creating it increments the root refcounts; deleting it decrements those roots and recursively releases newly unreachable blocks. Snapshot deletion is atomic but may require bounded background batches later; v1 may perform it synchronously.

A crash before superblock publication can leave unreachable allocated blocks. Offline `fsck` performs mark-and-sweep:

1. Select the highest valid superblock.
2. Mark blocks 0 and 1, the live roots, every snapshot root, and all transitively referenced metadata/data blocks.
3. Validate pointer bounds, generations, checksums, ordering, inode references, extent ranges, and link counts.
4. Compare the mark set with the allocator bitmap.
5. In report mode, describe leaks and multiply allocated blocks. With `--reclaim`, write a rebuilt CoW bitmap and commit a new generation.

---

## 5. NFSv4.0 protocol design

### 5.1 Persistent filehandles

Use an opaque fixed-format handle containing:

```text
{ fsid, inode_number, inode_generation, file_type, handle_checksum }
```

Derive `fsid` from the filesystem UUID. Inode numbers are stable, so handles survive process restarts. On `PUTFH`, verify the handle checksum and fsid, look up the inode, compare its generation and type, and return `NFS4ERR_STALE` on mismatch. Do not expose physical block addresses or B-tree keys.

### 5.2 COMPOUND execution

Decode the entire bounded request, then execute operations in order while tracking the current and saved filehandles. Stop at the first failing operation and return prior successful results plus the failure, as required by NFSv4. Map read-only compounds to a pinned `View`. A mutating compound uses one `Txn` so related operations can commit atomically when stable semantics require it.

Attribute handling is bitmap-driven. Maintain one table describing each supported attribute's bit number, XDR codec, read/write status, and engine mapping. Reject unsupported required attributes accurately; do not silently fabricate values.

### 5.3 Client, open, and lock state

Maintain these in-memory tables:

- **Clients:** client ID, verifier/confirmation state, lease deadline, and owned state.
- **Open owners:** client ID, owner bytes, next sequence ID, replay cache, and open stateids.
- **Opens:** inode, access/share-deny modes, stateid generation, and reference count.
- **Lock owners:** client/open association, sequence ID, and byte-range intervals.

Use a 90-second lease initially. `SETCLIENTID` creates or updates an unconfirmed record; `SETCLIENTID_CONFIRM` activates it. Successful stateful operations renew the lease, as does `RENEW`. Enforce open-owner and lock-owner sequence IDs and cache the last response needed for replay handling.

Represent byte-range locks per inode in an ordered interval structure. Detect overlap across distinct lock owners; compatible ranges by the same owner may merge. `LOCKU`, `CLOSE`, `RELEASE_LOCKOWNER`, and client expiry release state according to protocol rules. A background reaper revokes all opens and locks for expired clients. There are no delegations in v1.

Blocking locks are deferred unless client interoperability proves they are required; v1 can return a denied/conflict result immediately. Persisting client/open/lock state and implementing a restart grace period remains an open decision.

### 5.4 Concurrency model

Start simple:

- One acceptor plus a bounded worker pool for RPC connections.
- Concurrent read-only engine views.
- One serialized writer transaction/commit path.
- State-table locks separate from the storage transaction lock; define one lock order and assert it in debug builds.
- No response indicating stable storage until the relevant transaction commit completes.

This keeps the disk correctness model independent from network concurrency. Optimize only after profiling shows whether contention is in XDR, tree lookup, copying, or sync latency.

### 5.5 Suggested source layout

```text
src/
  block/        block_store, image/device backends, checksum
  format/       endian codecs, superblock, block headers
  btree/        generic tree, cursor, split/merge, model adapter
  alloc/        free bitmap, refcounts, recursive release
  txn/          transaction, dirty set, commit coordinator
  fs/           inode, directory, extent, snapshot, Engine API
  rpc/          TCP record marking, ONC RPC, AUTH_SYS
  nfs4/         XDR codecs, attributes, COMPOUND, state, locks
  server/       lifecycle, worker pool, lease reaper
cmd/
  cownfsd       server
  mkcownfs      formatter
  cownfs-fsck   verifier/reclaimer
  cownfs-dump   offline inspector
tests/
  unit/ model/ crash/ nfs/ fixtures/
```

Keep on-disk codecs distinct from in-memory types. Golden-byte tests should make format drift obvious in review.

---

## 6. Implementation phases and gates

### P0 — Blocks, superblocks, and tools

Implement the block store, checksum abstraction, dual superblocks, format codecs, `mkcownfs`, and an `fsck` traversal skeleton.

**Gate:** format an image; dump both superblocks; flip one bit in the active slot; verify corruption is detected and mount selects the older valid slot where applicable.

### P1 — CoW B-tree and lifetime accounting

Implement lookup, ordered iteration, insert, replace, delete, split/merge, checked child dereference, refcounts, and recursive free.

**Gate:** run one million randomized operations against a Rust `BTreeMap` model; repeatedly create/delete root snapshots; after every cycle assert `allocated blocks == blocks reachable from all roots`.

### P2 — Files, directories, extents, and recovery

Implement inode/directory/extent trees, the engine API, partial writes, truncation, hard links, symlinks, the allocator bitmap, and full mark-and-sweep `fsck`.

**Gate:** create 10,000 files with deterministic random data and verify all bytes and metadata; send `kill -9` at random points during load, remount at the last committed generation, and require `fsck` to report clean.

### P3 — Snapshots

Implement snapshot create/delete/list, root reference changes, CoW divergence, and reclaim after deletion.

**Gate:** snapshot a populated image, overwrite all live data, and verify the snapshot still returns the old bytes; delete it and prove its exclusive blocks are reclaimed.

### P4 — RPC/XDR and read-only NFS

Implement TCP record framing, ONC RPC, AUTH_SYS, XDR, filehandles, read-only COMPOUND operations, `READDIR`, and attributes.

**Gate:** Linux `mount -t nfs -o vers=4` succeeds; `ls -R` matches an engine-side manifest; the applicable read-only pynfs subset passes.

### P5 — NFS mutation and durability

Implement regular-file creation through `OPEN`, directory/symlink creation through `CREATE`, plus `REMOVE`, `RENAME`, `LINK`, `WRITE`, `COMMIT`, and `SETATTR`. Connect NFS stable-write semantics to the transaction commit path.

**Gate:** untar a Linux kernel source tree over NFS and diff it byte-for-byte and metadata-wise against the source; the applicable pynfs write subset passes.

### P6 — Opens, locks, and leases

Implement client confirmation, stateids, open/share state, locking, sequence IDs, replay handling, lease renewal, and the expiry reaper.

**Gate:** two clients exhibit correct open/share and byte-range lock conflicts; killing one client and waiting past the configured lease releases its locks; applicable pynfs state tests pass.

### P7 — Hardening and measurement

Add deterministic fault injection, malformed-XDR tests, `fsck --reclaim`, long-running workloads, and operational diagnostics.

**Gate:** crash injection passes across every commit boundary; multi-hour create/write/rename/delete/snapshot soak tests leave a clean image; `fio` runs over NFS are recorded for context, not treated as a competition with kernel filesystems.

No phase is complete merely because its happy path works. Its gate becomes a regression target in CI before the next phase begins.

---

## 7. Testing strategy

**pynfs, maintained by the Linux NFS project, is the NFS correctness gold standard for `cownfs`.** It speaks the protocol directly and exercises operation sequencing, state, locking, replay, and error results that normal kernel-client workloads may never send. RFC 7530 remains normative—the pynfs maintainers explicitly caution that an individual test failure is not automatically proof of a server bug. Pin a specific revision, run only explicitly unsupported feature exclusions, and keep the exclusion list reviewed and small. A Linux kernel NFS server baseline helps distinguish suite assumptions from `cownfs` failures.

The remaining test pillars are:

- **B-tree model testing:** from P1 onward, generate insert/update/delete/scan/snapshot operations and compare every observable result with a Rust `BTreeMap`-based persistent model. Shrink failing seeds and preserve them as fixtures.
- **Crash injection:** annotate commit stages and randomly terminate with `SIGKILL`; remount, identify the selected generation, and compare against the last acknowledged stable state. Include torn and reordered writes in a fake block backend, not only process kills against a real file.
- **Format tests:** golden encoded blocks, endian round-trips, checksum failures, invalid offsets, overlapping items, cycles, out-of-range pointers, and excessive tree depth.
- **Engine semantics:** link counts, rename replacement rules, directory cycles, sparse reads, truncate extension/shortening, timestamps, stale inode generations, and snapshot isolation.
- **Protocol fuzzing:** fuzz RPC record marking, XDR lengths/unions, compound op sequences, attribute bitmaps, filehandles, and stateids. The decoder must fail boundedly without touching storage.
- **System tests:** Linux and macOS client mounts; untar/diff; concurrent writers; lock contention; reconnects; server restart; multi-hour soak.

Track three different acknowledgements in the test oracle: an unstable NFS reply, a stable NFS reply, and a published filesystem generation. After a crash, only the latter two may be expected to survive, and only when the stable reply was sent after successful publication.

---

## 8. Programming language decision

**Rust is the primary implementation language.** The storage format and module boundaries remain language-independent, but safety takes priority over initial implementation speed.

| Language | Main advantage | Main concern |
|---|---|---|
| Rust | Compile-time safety | Initial borrow friction |
| C++20 | Immediate velocity | Manual memory safety |
| C | Minimal runtime | Highest bug exposure |
| Go | Fast server work | GC in storage path |
| Zig | Explicit low-level model | Young ecosystem/tooling |

### Rust

Rust is the best fit for a filesystem whose mistakes can corrupt user data:

- **Untrusted network bytes:** the NFSv4 server hand-parses ONC RPC and XDR received from the network. Rust makes bounds-checked parsing and memory safety the default, so malformed lengths, unions, and compound operations cannot become buffer over-reads or use-after-free bugs in safe code.

- **Shared CoW ownership:** refcounted B-tree nodes, snapshot roots, path copying, deferred decrements, and recursive free are exactly the manual-ownership code where a use-after-free or double-free can silently destroy data. Rust's ownership model turns broad classes of these failures into compile-time errors.

- **Concurrent clients:** workers share committed views, NFS state tables, caches, and the serialized commit path. Rust's `Send` and `Sync` rules prevent non-thread-safe state from crossing worker boundaries and rule out data races in safe code.

Use arena allocation with generational indices—a `slotmap`-style design—for in-memory B-tree nodes rather than pointer-linked parent/child objects. A node reference is a typed index plus generation, so deleting and reusing a slot invalidates stale references. On-disk links remain checked block numbers with generation and checksum fields. This pattern keeps graph-like mutation explicit without fighting self-referential borrows.

Enums model XDR discriminated unions and NFS state transitions well. Slices and newtypes make bounded byte parsing explicit. Unsafe code should be unnecessary in the initial implementation; if later profiling justifies any, isolate it behind a small audited abstraction with safe property tests.

The honest cost is lower initial velocity while the author gains fluency, especially around B-tree mutation and transaction lifetimes. The server-side NFSv4 crate ecosystem is thin, so RPC/XDR remains custom. Those costs are accepted because safety is the governing requirement.

### C++20 alternative

C++20 remains the documented fallback. The author already has deep C++ experience from a roughly 31 KLOC VM with a GC and JIT, so it offers essentially zero learning curve and higher initial velocity. It also provides deterministic execution, no garbage-collector pauses, direct layout control, and mature sanitizers and fuzzers.

The trade is decisive: all memory safety, node lifetime, recursive release, buffer slicing, and cross-thread synchronization remain manual. RAII, smart pointers, sanitizers, fuzzing, and narrow mutation capabilities reduce risk but do not provide Rust's compile-time guarantees. Choose C++20 only if delivery speed is explicitly reprioritized above the safety-first decision.

### C

C gives maximal ABI and layout control with a tiny runtime, and its simplicity fits disk codecs. It offers no ownership help for complex tree and transaction lifetimes, however, and would recreate classes of bugs the design can avoid. It is a defensible choice only if minimalism itself is a project goal.

### Go

Go is attractive for TCP/RPC concurrency, tooling, fuzzing, and iteration speed. Its garbage collector and less direct allocation/layout control are awkward in the storage path, even if pauses are usually small. Existing NFS work may provide ideas, but the required NFSv4.0 server surface and custom XDR remain substantial.

### Zig

Zig offers explicit allocation, deterministic destruction, simple C interop, and good binary-format ergonomics without a GC. Its ecosystem and language/toolchain stability are weaker than C++ or Rust for a data-integrity project expected to live for years.

### Recommendation

Proceed with **stable Rust** for production code. Favor safe Rust throughout the block, tree, transaction, RPC/XDR, and NFS state layers. Use generational arena indices for mutable graph structures, explicit endian codecs for disk bytes, bounded parsers for wire bytes, and compile-time `Send`/`Sync` checks at concurrency boundaries.

C++20 remains the speed-first alternative, not a parallel prototype requirement. Revisit it only if Rust creates a measured blocker that cannot be solved without an unsafe surface larger than the equivalent audited C++ implementation.

---

## 9. Core invariants

These invariants should appear as assertions in debug builds and checks in `fsck`:

1. A committed block is never modified in place.
2. Every published pointer is in range and matches the child's checksum and expected generation.
3. A superblock is selectable only if its own checksum and all required root pointers validate.
4. A block is allocatable only if unreachable from the live roots and every snapshot root.
5. Every directory entry references an existing inode of the recorded type.
6. Every physical extent is allocated, non-overlapping, and referenced consistently.
7. An acknowledged `FILE_SYNC` write or successful `COMMIT` belongs to a published generation.
8. A filehandle resolves only when fsid, inode number, inode generation, and type all match.
9. NFS state transitions accept only valid client/owner sequence numbers and stateids.
10. All externally supplied lengths and counts are bounded before memory allocation or arithmetic.

---

## 10. Open questions

- **Block size:** fixed 4 KiB versus 16 KiB. Start with 4 KiB for common page/device alignment and lower small-write amplification; measure B-tree fanout and sequential metadata cost before freezing the format.
- **Checksum:** CRC32C versus xxHash64. CRC32C has hardware acceleration and filesystem precedent; xxHash64 has a wider result. Benchmark both through the abstraction and persist the choice.
- **Extent topology:** one global extent tree versus per-file extent trees. The global tree simplifies roots, snapshots, and fsck; per-file trees may improve locality for very large files but add root metadata.
- **Lease duration:** begin at 90 seconds, then tune test duration and recovery behavior without violating NFSv4.0 semantics.
- **Restart state:** persist clients/opens/locks and implement a grace period, or start clean in v1. This affects correctness after server restart and must be resolved before P6 is declared complete.
- **Inode width:** use `u64` initially unless there is a compelling format-density reason not to; decide before the first stable on-disk format.
- **Blocking locks:** return immediate conflict in v1 or queue blocking requests. Confirm what Linux and macOS clients exercise through pynfs and system tests.

---

## 11. Immediate next steps

1. Freeze an on-disk format v0 header with explicit endian codecs and feature flags.
2. Resolve and model-test the crash-consistent refcount representation before P1.
3. Create the Rust workspace, define checked block/index newtypes, and establish a no-`unsafe` baseline for P0.
4. Implement P0 with a fake block backend capable of torn, lost, and reordered writes.
5. Pin pynfs early and make one empty/read-only RPC response work before P4, so protocol risk is exposed while the storage engine is still small.

---

## References

- Ohad Rodeh, *B-trees, Shadowing, and Clones*: https://phroxy.z3bra.org/gopher.petergarner.net:70/9/The_TPN_Papers/Paper_-_B-Trees,_Shadowing_and_Clones.pdf
- Btrfs design documentation: https://btrfs.readthedocs.io/en/latest/dev/dev-btrfs-design.html
- RFC 7530, *Network File System (NFS) Version 4 Protocol*: https://www.rfc-editor.org/info/rfc7530/
- RFC 7531, *NFSv4 External Data Representation (XDR)*: https://datatracker.ietf.org/doc/html/rfc7531
- pynfs, the Linux NFS project's NFSv4 test suite: https://github.com/linux-nfs/pynfs
- ghostfs2, a small userspace CoW filesystem reference: https://github.com/quzopl/ghostfs2

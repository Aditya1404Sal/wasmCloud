# File-Backed mmap for Compiled WebAssembly Components

## Problem

When a WebAssembly component is compiled via `Component::new()`, wasmtime compiles the
`.wasm` bytecode to native machine code using Cranelift and stores the result in
**anonymous mmap'd memory** [[1]][[2]] (`r-xp [anon]`). This memory:

- Counts fully toward process RSS (Resident Set Size)
- Cannot be reclaimed by the kernel under memory pressure — it must be swapped [[4]]
- Accumulates linearly: `num_workloads × num_components × avg_compiled_size`
- Persists for the lifetime of the `Component` handle

For deployments with many components, this becomes the dominant source of memory usage.

This was confirmed by a wasmCloud developer [[17]], who noted that the compiled code is
"essentially the equivalent of `.text` sections from a dynamically loaded shared library,
except there's no file backing so it shows as anonymous" and suggested implementing "a
caching layer that backs wasmtime's compilation cache" — which is exactly what this change
does.

## Solution

wasmtime supports serializing compiled components to an ELF-based [[6]][[7]] `.cwasm`
format [[9]] and loading them back via `Component::deserialize_file()`, which uses a
**file-backed mmap** [[2]] instead of anonymous memory. File-backed pages:

- Can be **evicted by the kernel** [[3]] under memory pressure without swapping — the
  kernel simply re-reads from the backing file when needed
- Behave like `.text` sections [[6]] of a dynamically loaded shared library
- Show as named mappings in `/proc/PID/maps` (easier to diagnose)
- Are shared across processes if the same file is mapped (relevant for multi-process setups)

## Memory Formula

```
RSS = base_host_RSS
    + (num_workloads × num_components × compiled_pages_resident)  ← targeted by this change
    + sum_over_active_instances(initial_memory + memory_grown)    ← unchanged
```

This change targets the **second term** — the compiled native code produced by Cranelift.

**Before** (anonymous mmap): `compiled_pages_resident` = `avg_compiled_size` always. Every
byte of compiled code is pinned in RAM. The kernel cannot reclaim it without swapping.

**After** (file-backed mmap): `compiled_pages_resident` ranges from **0** (all evicted,
component idle) to `avg_compiled_size` (all hot). The kernel manages this automatically
based on actual access patterns.

The **linear memory term** is less expensive than it appears — see
[Understanding the pooling allocator](#understanding-the-pooling-allocator) below.

### Example

10 workloads, each with 3 components averaging 20MB compiled. 50 concurrent instances,
each with ~2 MB allocated at runtime:

| | Before | After (idle) | After (all active) |
|-|--------|-------------|-------------------|
| Compiled code | 10 × 3 × 20MB = **600MB pinned** | 10 × 3 × ~2MB = **~60MB** | 10 × 3 × 20MB = **600MB** |
| Linear memory | 50 × ~2MB = **~100MB** | 50 × ~2MB = **~100MB** | 50 × ~2MB = **~100MB** |
| **Total RSS** | **~700MB** | **~160MB** | **~700MB** |

### Understanding the pooling allocator

A common misconception: "1000 max instances × 4 GiB max memory = 4 TiB RSS." **This is
wrong.** The pooling allocator uses demand paging:

1. **Virtual address space reservation (free)** — On startup, the pool reserves virtual
   address space for all slots (default: 1000 × 4 GiB = ~4 TiB) [[15]]. This is just
   `mmap(PROT_NONE)` — **zero RSS** [[14]]. On x86_64 with 128 TiB userspace [[13]],
   a 4 TiB reservation is effectively free.

2. **Physical pages committed on demand** — Only pages actually touched become RSS [[5]].
   Instances start with minimal initial memory (e.g., 64 KiB) and grow via `memory.grow`.

3. **Decommit on instance drop** — Memory is returned via `madvise(MADV_DONTNEED)` [[16]].
   The virtual reservation stays for pool reuse.

> **Config:** The 4 GiB default is wasmtime's `PoolingAllocationConfig::memory_reservation`
> [[15]], overridable via `WASMTIME_POOLING_MAX_MEMORY_SIZE`. Max instances default to 1000
> (see `EngineBuilder::build()`). `linear_memory_keep_resident` (default: 0) controls
> bytes kept resident after deallocation, configurable via
> `WASMTIME_POOLING_LINEAR_MEMORY_KEEP_RESIDENT`.

## Implementation

### What Changed

**File:** `crates/wash-runtime/src/engine/mod.rs`

1. **`Engine` struct** — Added `compiled_cache_dir: Option<PathBuf>` field
2. **`EngineBuilder`** — Added `with_compiled_cache_dir(dir)` builder method
3. **`load_component_bytes()`** — Now delegates to `load_or_compile()` which implements
   a compile-serialize-reload pipeline
4. **`load_or_compile()`** (new) — Core logic:
   - If a `.cwasm` file exists for this digest → `Component::deserialize_file()` (file-backed mmap)
   - If not → `Component::new()` → `serialize()` → write `.cwasm` → `deserialize_file()` to reload as file-backed
   - All failures gracefully fall back to the in-memory compiled component

### How to Enable

```rust
let engine = Engine::builder()
    .with_compiled_cache_dir("/var/cache/wasmcloud/compiled")
    .build()?;
```

When `compiled_cache_dir` is **not set**, behavior is identical to before — components
compile into anonymous mmap with no disk I/O.

### Cache Key

The cache key is the component's **digest** (e.g., `sha256:a1b2c3...`), which is already
passed through the component loading pipeline from OCI registries. This ensures:

- Same component bytes → same cache file (content-addressable)
- Different component bytes → different cache file (no collisions)

### Version Safety

`.cwasm` files are **engine-version-specific**. wasmtime embeds version and configuration
metadata in the serialized format and validates it on `deserialize_file()`. If the
wasmtime version changes (e.g., after a wash-runtime upgrade):

1. `deserialize_file()` returns an error
2. The stale `.cwasm` file is deleted
3. The component is recompiled from source and a new `.cwasm` is written
4. This happens transparently with a warning log

### Failure Modes

Every step has a graceful fallback:

| Failure | Behavior |
|---------|----------|
| Cannot create cache directory | `build()` returns error (fail-fast) |
| Cannot write `.cwasm` to disk | Warning logged, uses in-memory compiled component |
| Cannot serialize component | Warning logged, uses in-memory compiled component |
| Cannot deserialize cached file | Warning logged, deletes stale file, recompiles |
| Cannot reload from written file | Warning logged, uses in-memory compiled component |

### Safety

`Component::deserialize_file()` is `unsafe` because wasmtime trusts the file contents —
a tampered `.cwasm` could cause arbitrary code execution. This is acceptable here because:

- We only deserialize files we serialized ourselves in the same process/engine
- The cache directory should have restrictive permissions (e.g., `0700`)
- Production deployments should restrict write access to the cache directory

## Observability

- `DEBUG` — cache hits, compilation starts, successful writes
- `WARN` — fallback paths (serialization failures, stale artifacts, write errors)

All log messages include the file path and digest for correlation.

## File Layout

```
<compiled_cache_dir>/
├── sha256-a1b2c3d4e5f6...cwasm
├── sha256-f6e5d4c3b2a1...cwasm
└── ...
```

## Runtime Performance

**Will invoking a component be slower?** No. After the first access, performance is
identical to the old approach.

`deserialize_file()` calls `mmap()` with `MAP_PRIVATE` — this sets up virtual mappings but
**does not read the file into RAM** (demand paging [[5]]). On first invocation, the CPU
triggers a page fault (~100-500us from SSD) and the kernel reads that 4KB page from disk.
After that, the page is in RAM at full native speed.

| Phase | File-backed (new) | Anonymous (old) |
|-------|-------------------|-----------------|
| First function call | Page fault (~100-500us from SSD) | Already resident |
| Subsequent calls | Full native speed | Full native speed |
| Under memory pressure | Pages evicted, re-read from file | Pages swapped (slow) |
| Rarely-used component | Pages evicted, saves RAM | Pages stay in RAM or go to swap |

The page-fault latency is only observable when a component is called very infrequently
(pages get evicted between calls) AND you need sub-millisecond latency on every invocation.

## Trap Tables

Trap tables are metadata structures embedded in `.cwasm` files that enable wasmtime to
convert hardware faults into WebAssembly traps [[10]][[11]][[12]].

Rather than inserting explicit bounds-check branches before every memory access, wasmtime
uses **signal-based trap handling** [[11]]: Cranelift records a trap table entry
`(native_code_offset, trap_code)` for each instruction that could fault. At runtime, a
hardware signal (e.g., `SIGSEGV`) is caught, the faulting instruction is looked up in the
trap table, and the signal is converted into a clean WebAssembly trap with a proper stack
trace.

The trap table is serialized as an ELF section (`.wasmtime.traps`) inside the `.cwasm`
file [[9]][[10]]. Alongside it, **address maps** provide native-to-wasm offset mappings
for meaningful stack traces. Both are file-backed and can be evicted/re-read like code
pages.

## Kubernetes OOM Behavior

**This change reduces OOM risk but does not eliminate it.**

Under gradual memory pressure, the kernel prefers evicting file-backed pages (cheap) over
killing processes. With anonymous mmap (old approach), compiled code pages could only be
freed by swapping or killing the process.

**What does NOT improve:**

- **Linear memory is always anonymous** — if OOM is caused by wasm heap growth, this
  doesn't help (though the pooling allocator only commits actually-used memory; see
  [pooling allocator](#understanding-the-pooling-allocator))
- **Sudden memory spikes** — the OOM killer may fire before the kernel can reclaim pages
- **`limits.memory` is a hard wall** — reclaim may not free memory fast enough

**Kubernetes recommendations:**

- Set `requests.memory` to cover your expected working set (base + active instances)
- Set `limits.memory` with headroom for page cache (compiled code pages)
- Use `with_compiled_cache_dir()` pointing to an emptyDir volume or writable layer
- Monitor via `/sys/fs/cgroup/memory.stat` (`active_file` = reclaimable file-backed pages)

## Tradeoffs

**Benefits:**

1. **Lower RSS** — file-backed pages are evicted freely under pressure; anonymous pages must be swapped
2. **Faster warm starts** — loads pre-compiled `.cwasm` via mmap instead of recompiling via Cranelift
3. **Better observability** — named mappings in `/proc/PID/maps` instead of `r-xp [anon]`
4. **Cross-process page sharing** — same `.cwasm` file = shared physical pages
5. **Fully opt-in** — no behavior change unless `compiled_cache_dir` is configured

**Costs:**

1. **First-load penalty** — cache miss does: `Component::new()` → `serialize()` → `fs::write()` → `deserialize_file()`. Amortized on subsequent loads.
2. **Disk space** — `.cwasm` files are **2-5x larger** than `.wasm` source. No automatic eviction.
3. **Code complexity** — ~80 lines with fallback paths vs the old 15-line `load_component_bytes()`
4. **`unsafe` code** — `deserialize_file()` trusts file contents. See [Safety](#safety).
5. **Latency jitter** — page faults under memory pressure add variance. See [Runtime Performance](#runtime-performance).
6. **Filesystem dependency** — read-only containers, constrained tmpfs, or network mounts may cause issues

## Potential Pitfalls

1. **Engine configuration mismatch** — The cache key is the component digest only, not the
   engine config. Two engines with different `wasmtime::Config` settings loading the same
   component will share the `.cwasm` file. wasmtime validates some config fields on
   deserialize but not all. A safer approach would include an engine config fingerprint in
   the cache key.

2. **Race conditions** — Multiple processes writing to the same cache directory may race on
   `.cwasm` files. The implementation uses `fs::write()` (not atomic rename), so a reader
   could see a partial file. Handled gracefully (deserialization fails → recompile).

3. **Unbounded disk growth** — No cache eviction. Every unique component leaves a `.cwasm`
   file forever. Frequent deployments will eventually fill the disk.

4. **Components without digest** — Components loaded without a digest bypass the disk cache
   and always use anonymous mmap. The memory benefit only applies to OCI-sourced components
   with known digests.

## Design Decisions

1. **Compile-serialize-reload on first load** — We reload from the file immediately so the
   *current* load also gets file-backed pages, not just future ones.

2. **Digest as sole cache key** — Simple and content-addressable. Chosen over including an
   engine config hash because wash-runtime typically uses a single engine configuration.

3. **Graceful fallback everywhere** — Every disk operation falls back to old behavior. Disk
   issues degrade to anonymous mmap rather than causing errors.

4. **Synchronous I/O** — `load_or_compile()` uses `std::fs` because it runs inside moka's
   `try_get_with()` closure (synchronous). I/O is bounded to one file per cache miss.

5. **Two-level cache (moka + disk)** — Moka hit returns the `Component` handle instantly.
   Disk cache is only consulted on moka miss.

6. **Flat directory structure** — All `.cwasm` files in one directory. Could degrade on
   filesystems with many entries (e.g., ext4 without dir_index), but fine for typical
   deployments.

7. **No atomic writes** — `fs::write()` directly, not write-to-temp + rename. Partial files
   are detected and handled on next load (deserialize fails → delete → recompile).

8. **Fail-fast only at build time** — `EngineBuilder::build()` fails if the cache directory
   cannot be created. All runtime failures are warnings with fallbacks.

## Future Improvements

- **Cache eviction**: TTL-based or LRU eviction for `.cwasm` files to bound disk usage
- **Atomic writes**: Write-to-temp + rename for crash safety
- **Upstream wasmtime fix**: The built-in `wasmtime_cache` crate uses `load_code_bytes`
  (anonymous mmap) rather than `load_code_file` (file-backed mmap). Contributing a fix
  upstream would make the transparent cache as memory-efficient as `deserialize_file()`.

## References

**Memory management and mmap:**

- **[1]** [Linux Kernel Memory Management Concepts](https://docs.kernel.org/admin-guide/mm/concepts.html) — Anonymous vs file-backed memory mappings
- **[2]** [mmap(2) — Linux manual page](https://man7.org/linux/man-pages/man2/mmap.2.html) — `mmap()` flags, behavior, and mapping types
- **[3]** [Page Cache Eviction and Page Reclaim](https://biriukov.dev/docs/page-cache/4-page-cache-eviction-and-page-reclaim/) — How the kernel evicts file-backed pages under memory pressure
- **[4]** [In Defence of Swap](https://chrisdown.name/2018/01/02/in-defence-of-swap.html) — Why anonymous pages require swap while file-backed pages can be evicted freely
- **[5]** [What They Don't Tell You About Demand Paging in School](https://offlinemark.com/2020/10/14/demand-paging/) — Page fault mechanics and demand paging for mmap'd files

**ELF format:**

- **[6]** [Executable and Linkable Format — Wikipedia](https://en.wikipedia.org/wiki/Executable_and_Linkable_Format) — ELF structure: headers, sections (`.text`, `.data`), segments
- **[7]** [elf(5) — Linux manual page](https://man7.org/linux/man-pages/man5/elf.5.html) — ELF format specification
- **[8]** [How programs get run: ELF binaries — LWN.net](https://lwn.net/Articles/631631/) — How the kernel loads ELF binaries via mmap

**Wasmtime and trap handling:**

- **[9]** [Pre-Compiling Wasm — Wasmtime docs](https://docs.wasmtime.dev/examples-pre-compiling-wasm.html) — Serializing compiled components to `.cwasm` files
- **[10]** [Wasmtime Architecture](https://docs.wasmtime.dev/contributing-architecture.html) — Internals including trap handling and compiled artifact structure
- **[11]** [Making WebAssembly and Wasmtime More Portable](https://bytecodealliance.org/articles/wasmtime-portability) — Signal-based trap handling across platforms
- **[12]** [Exceptions in Cranelift and Wasmtime](https://cfallin.org/blog/2025/11/06/exceptions/) — Exception and trap management in Cranelift

**Virtual address space and overcommit:**

- **[13]** [x86_64 Memory Management — Linux Kernel docs](https://www.kernel.org/doc/html/v5.8/x86/x86_64/mm.html) — 48-bit virtual addresses, 128 TiB userspace
- **[14]** [Overcommit Accounting — Linux Kernel docs](https://www.kernel.org/doc/html/latest/mm/overcommit-accounting.html) — Why reserving virtual address space is free

**Wasmtime pooling allocator:**

- **[15]** [PoolingAllocationConfig — Wasmtime API docs](https://docs.wasmtime.dev/api/wasmtime/struct.PoolingAllocationConfig.html) — Pooling allocator config including `memory_reservation` default
- **[16]** [memfd/madvise-based CoW pooling allocator — wasmtime PR #3697](https://github.com/bytecodealliance/wasmtime/pull/3697) — `madvise(MADV_DONTNEED)` for memory decommit

**Community:**

- **[17]** [wasmCloud Slack thread](https://wasmcloud.slack.com/archives/C02LZ3L0X4H/p1770992639388099) — Developer confirming the memory formula and suggesting a compilation caching layer

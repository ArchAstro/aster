# Pitfalls Research: Build Orchestration Tools

**Domain:** Build orchestration CLI tool (Aster - Rust-based dependency graph auto-detection)
**Researched:** 2026-01-22
**Overall confidence:** HIGH (multiple authoritative sources cross-referenced)

---

## Critical Pitfalls

These mistakes cause rewrites, major delays, or fundamental architecture problems.

### 1. Inadequate Cycle Detection

- **What goes wrong**: Build systems assume the dependency graph is a DAG (Directed Acyclic Graph). If cycles exist and aren't caught, the build enters infinite loops, produces incorrect ordering, or crashes cryptically. A well-formed build must not have cycles, but human errors occur and the build system must catch and report them clearly.

- **Warning signs**:
  - Hangs during dependency resolution with no output
  - Memory usage grows unbounded during graph traversal
  - Non-deterministic build ordering across runs
  - Users report "it works sometimes" behavior

- **Prevention**:
  - Implement cycle detection using DFS with recursion stack (O(V+E) complexity)
  - Use Tarjan's algorithm to identify all strongly connected components
  - Detect cycles BEFORE attempting topological sort, not during
  - Provide actionable error messages showing the exact cycle path (e.g., "A -> B -> C -> A")
  - Consider incremental cycle detection for dynamic graph updates

- **Recovery**: If shipped without proper cycle detection, add it as hotfix priority. Users with cycles in configs will be completely blocked.

- **Phase relevance**: Core engine phase - this is foundational. Cannot defer.

**Sources**: [Cycle Detection Algorithms](https://www.geeksforgeeks.org/dsa/topological-sorting/), [Dependency Graph Wikipedia](https://en.wikipedia.org/wiki/Dependency_graph)

---

### 2. Cache Invalidation Bugs

- **What goes wrong**: Build caching is essential for performance, but incorrect invalidation leads to "stale cache" bugs - the program "mostly works" but produces subtle, hard-to-reproduce errors. As the saying goes: "There are only two hard things in computer science: cache invalidation and naming things."

- **Warning signs**:
  - "Works after clean rebuild" reports from users
  - Non-deterministic test failures
  - Changes don't seem to take effect
  - Race conditions between concurrent builds

- **Prevention**:
  - Track ALL inputs that affect a cached result (file content, timestamps, config, environment)
  - Use content hashes (not just timestamps) for invalidation - like Bazel's approach
  - When invalidating on write, DELETE cache entries rather than UPDATE them (idempotent)
  - Implement cache versioning - bump version when cache format changes
  - Test cache invalidation explicitly: change input -> verify cache miss
  - Consider delayed double-deletion for race condition protection

- **Recovery**: Add cache-busting flag (--no-cache) immediately. Then systematically audit all cache dependencies.

- **Phase relevance**: Performance/caching phase. Design cache key strategy early even if implementation is later.

**Sources**: [Cache Invalidation Strategies](https://softbuilds.medium.com/cache-invalidation-strategies-that-dont-bite-you-later-bde3415687e5), [Facebook Engineering - Cache Made Consistent](https://engineering.fb.com/2022/06/08/core-infra/cache-made-consistent/)

---

### 3. Full Rebuild on Every Change

- **What goes wrong**: Without proper dependency tracking, a change to ANY file triggers rebuild of EVERYTHING. This is the #1 complaint about monorepo build tools. One team reported: "Why does a change to a README file cause a full rebuild of our entire platform?"

- **Warning signs**:
  - Build times grow linearly with project size
  - Small changes take minutes instead of seconds
  - CI pipelines become bottleneck (20-45 minute builds reported)
  - Developers avoid running builds locally

- **Prevention**:
  - Build a true dependency DAG from the start, not a flat task list
  - Implement affected-target analysis: which targets actually depend on changed files?
  - Use file content hashing, not just existence checks
  - Support parallel execution of independent targets
  - Test with realistic monorepo-scale projects early (1000+ nodes)

- **Recovery**: Major architectural work. Usually requires adding dependency tracking layer. Teams report 3-5x speedups after fixing this.

- **Phase relevance**: Core architecture decision in Phase 1. Cannot be bolted on later without significant rework.

**Sources**: [Solving Monorepo Hell with Bazel](https://medium.com/@erfan.mohebi/solving-monorepo-hell-with-bazel-a-deep-dive-into-modern-build-systems-f70c831bb227), [InfoQ - Monorepo Mistakes](https://www.infoq.com/presentations/monorepo-mistakes/)

---

### 4. Cross-Platform Path Handling Failures

- **What goes wrong**: Hardcoded path separators, case sensitivity assumptions, and symlink handling cause the tool to work on developer machines but fail in CI or on other platforms. Windows is particularly problematic.

- **Warning signs**:
  - "Works on Mac, fails on Windows" reports
  - Path-related panics with backslashes/forward slashes
  - Symlink resolution errors on Windows
  - Unicode path failures

- **Prevention**:
  - Use `std::path::Path` and `PathBuf` consistently - never string concatenation for paths
  - Test on all three platforms in CI from day one
  - Handle symlinks explicitly - they behave very differently on Windows (requires Developer Mode or admin)
  - Normalize paths before comparison (case-insensitive on Windows/macOS by default)
  - Use `canonicalize()` carefully - it resolves symlinks which may not be desired

- **Recovery**: Path handling is pervasive. Fixing post-facto requires touching most of the codebase.

- **Phase relevance**: Core infrastructure from Phase 1. Establish path handling conventions before writing file I/O code.

**Sources**: [Semgrep - Cross-Platform Tool Considerations](https://semgrep.dev/blog/2025/five-considerations-when-building-cross-platform-tools-for-windows-and-macos/), [Symlinks in Windows](https://blogs.windows.com/windowsdeveloper/2016/12/02/symlinks-windows-10/)

---

### 5. Config Parser Silent Failures

- **What goes wrong**: YAML's implicit typing and TOML's nested section handling cause configs to parse "successfully" but with wrong values. The app crashes later with unhelpful error messages far from the actual problem.

- **Warning signs**:
  - Values silently converted to wrong types (YAML: `version: 1.10` becomes float `1.1`)
  - Boolean coercion surprises (YAML: `country: NO` becomes `false`, not string "NO")
  - Nested config sections not properly captured
  - Whitespace-sensitivity errors in YAML

- **Prevention**:
  - Use TOML for Rust projects (explicit typing, no indentation sensitivity)
  - Validate parsed config against a schema immediately after parsing
  - Use `serde` with strict mode - fail on unknown fields
  - Provide line/column numbers in all config error messages
  - Test config parsing with intentionally malformed inputs

- **Recovery**: Add schema validation layer. May require config format migration.

- **Phase relevance**: Parser phase. Get this right before users create configs.

**Sources**: [Parsing Config Files The Right Way](https://dmerej.info/blog/post/parsing-config-files-the-right-way/), [JSON vs YAML vs TOML Comparison](https://dev.to/jsontoall_tools/json-vs-yaml-vs-toml-which-configuration-format-should-you-use-in-2026-1hlb)

---

## Moderate Pitfalls

These cause delays, technical debt, or degraded user experience.

### 6. Unhelpful Error Messages

- **What**: Generic errors like "Build failed" or stack traces instead of actionable guidance. Users cannot self-diagnose problems.

- **Why it happens**: Developers focus on happy path. Errors are afterthought. Technical details bubble up instead of user-friendly messages.

- **How to avoid**:
  - Every error must answer: What happened? Why? How to fix it?
  - Include file paths, line numbers, and context in errors
  - Use `miette` or `ariadne` crates for rich diagnostic output
  - Test error messages as part of UX, not just error existence
  - Never show raw panic messages to users

- **Phase relevance**: UX phase, but error handling patterns should be established early.

**Sources**: [NN/g Error Message Guidelines](https://www.nngroup.com/articles/error-message-guidelines/), [Rust CLI Error Handling](https://rust-cli.github.io/book/tutorial/errors.html)

---

### 7. File Watcher Limits on Linux

- **What**: Linux's inotify has default limits (8192 watches). Large monorepos exceed this, causing "ENOSPC: System limit for number of file watchers reached" errors.

- **Why it happens**: Developers test on small projects. Production monorepos have 10,000+ files.

- **How to avoid**:
  - Document the limit and how to increase it (`fs.inotify.max_user_watches`)
  - Implement watch filtering - exclude `node_modules`, `.git`, build outputs
  - Consider polling fallback with degraded performance warning
  - Use Git's FSMonitor integration for large repos (available since Git 2.37)

- **Phase relevance**: Watch mode feature phase.

**Sources**: [JetBrains - inotify Limits](https://intellij-support.jetbrains.com/hc/en-us/articles/15268113529362-Inotify-Watches-Limit-Linux), [GitHub Blog - FSMonitor](https://github.blog/engineering/infrastructure/improve-git-monorepo-performance-with-a-file-system-monitor/)

---

### 8. Auto-Detection Heuristics Gone Wrong

- **What**: Automatic project type detection makes wrong guesses, leading to incorrect build commands or missed projects entirely.

- **Why it happens**: Heuristics based on file presence (e.g., "has package.json = Node project") fail when projects have multiple build systems or non-standard layouts.

- **How to avoid**:
  - Make auto-detection overridable via explicit config
  - Log detection reasoning so users can debug
  - Handle multi-language projects (a directory can be both Go AND has a Makefile)
  - Fail clearly when detection is ambiguous rather than guessing wrong
  - Test detection with real-world project structures, not just clean examples

- **Phase relevance**: Parser/detection phase.

**Sources**: [CodeQL Auto-Detection Issues](https://github.com/github/codeql/issues/17983), [NuGet MSBuild Detection Bug](https://github.com/NuGet/Home/issues/7621)

---

### 9. Concurrent Access Race Conditions

- **What**: Multiple processes (parallel builds, watch mode + manual build) corrupt shared state like lock files, caches, or output directories.

- **Why it happens**: File system operations aren't atomic. TOCTOU (time-of-check-time-of-use) races between checking file existence and acting on it.

- **How to avoid**:
  - Use proper file locking (`flock` on Unix, `LockFile` on Windows)
  - Implement lock files with PID and timeout for stale lock detection
  - Make operations idempotent where possible
  - Use atomic rename pattern: write to temp file, then rename
  - Test concurrent execution explicitly

- **Phase relevance**: Any phase with file I/O, especially parallelization and watch mode.

**Sources**: [File Locking for Security](https://www.informit.com/articles/article.aspx?p=23947&seqNum=6), [Avoiding Race Conditions](https://dwheeler.com/secure-programs/Secure-Programs-HOWTO/avoid-race.html)

---

### 10. Memory Blowup on Large Graphs

- **What**: In-memory graph representation consumes gigabytes for large monorepos. Tool becomes unusable or OOMs.

- **Why it happens**: Naive graph storage (e.g., adjacency matrix instead of adjacency list) or keeping all node data in memory when only structure is needed for traversal.

- **How to avoid**:
  - Use adjacency lists (O(V+E) space) not matrices (O(V^2) space)
  - Lazy-load node metadata only when needed
  - Consider memory-mapped files for very large graphs
  - Profile memory usage with realistic project sizes early
  - Set reasonable limits and fail gracefully with helpful message

- **Phase relevance**: Core engine phase - data structure choices are foundational.

**Sources**: [ACM - Topological Sorting for Large Graphs](https://dl.acm.org/doi/10.1145/2133803.2330083), [Monorepo Tools Comparison](https://monorepo.tools/)

---

## Minor Pitfalls

These cause annoyance but are fixable without major rework.

### 11. Missing Progress Indicators

- **What**: Long operations show no output, making users think the tool is hung.

- **Prevention**: Add progress bars (`indicatif` crate), spinner for unknown duration, or periodic status output. Cancel on Ctrl+C cleanly.

---

### 12. Inconsistent Config Locations

- **What**: Users don't know where to put config files. Tool looks in different places on different platforms.

- **Prevention**: Follow XDG spec on Linux, standard locations on macOS/Windows. Document clearly. Support `--config` flag override.

---

### 13. No Dry-Run Mode

- **What**: Users can't preview what the tool will do before it does it. Especially painful for destructive operations.

- **Prevention**: Add `--dry-run` flag from the start. Much harder to add retroactively.

---

### 14. Breaking Changes in Config Format

- **What**: Config format evolves, breaking existing user configs on upgrade.

- **Prevention**: Version config files explicitly. Provide migration scripts or auto-migration with backup.

---

### 15. Ignoring .gitignore Patterns

- **What**: Tool processes files that should be ignored (build outputs, dependencies), slowing down and producing wrong results.

- **Prevention**: Respect `.gitignore` by default. Use `ignore` crate for efficient gitignore parsing.

---

## Edge Cases to Handle

These specific scenarios frequently cause bugs if not explicitly addressed:

### Graph Edge Cases
- [ ] Empty project (no dependencies) - should still work
- [ ] Single node with self-dependency (immediate cycle)
- [ ] Disconnected components in the graph
- [ ] Very deep dependency chains (1000+ levels) - stack overflow risk
- [ ] Diamond dependencies (A -> B, A -> C, B -> D, C -> D)
- [ ] Multiple roots (no single entry point)

### File System Edge Cases
- [ ] Paths with spaces, unicode characters, emoji
- [ ] Symlink loops (A -> B -> A)
- [ ] Broken symlinks (target doesn't exist)
- [ ] Files becoming directories between check and use
- [ ] Read-only files and directories
- [ ] Network file systems (NFS, SMB) with different semantics
- [ ] Case sensitivity mismatches (macOS HFS+ is case-insensitive by default)

### Config Parsing Edge Cases
- [ ] Empty config file
- [ ] Config file with only comments
- [ ] Very large config files (10MB+)
- [ ] Circular includes/references in config
- [ ] Invalid UTF-8 in config
- [ ] Config file modified during parsing

### Platform Edge Cases
- [ ] Windows paths with drive letters (C:\)
- [ ] UNC paths (\\server\share)
- [ ] Windows reserved names (CON, PRN, NUL, etc.)
- [ ] macOS resource forks
- [ ] Linux maximum path length (4096 bytes)
- [ ] Windows maximum path length (260 characters by default, longer with manifest)

### Concurrency Edge Cases
- [ ] Same target requested multiple times simultaneously
- [ ] Dependency modified while building dependent
- [ ] Build interrupted mid-way (Ctrl+C)
- [ ] System crash leaving partial state
- [ ] Clock skew between machines (distributed builds)

---

## Phase-Specific Warnings

| Phase | Likely Pitfall | Mitigation |
|-------|---------------|------------|
| Core Engine | Cycle detection, memory blowup | Use DFS with recursion stack, adjacency lists |
| Config Parser | Silent type coercion, unhelpful errors | TOML + schema validation + line numbers |
| Auto-Detection | Wrong guesses, missed projects | Overridable detection, clear logging |
| Caching | Invalidation bugs, race conditions | Content hashing, delete-not-update, file locking |
| Watch Mode | inotify limits, concurrent access | Filter watches, document limits, proper locking |
| Cross-Platform | Path handling, symlinks | Platform-agnostic APIs, CI on all platforms |
| Performance | Full rebuilds, memory | Affected-target analysis, lazy loading |
| UX | Cryptic errors, no progress | Rich diagnostics, progress indicators |

---

## Summary Recommendations for Aster

Given Aster's specific concerns (parsing reliability, cycle detection, scale, cross-platform, UX), prioritize:

1. **Cycle Detection** (Critical): Implement robustly in core engine. Show exact cycle path in errors.

2. **Cross-Platform Paths** (Critical): Use Rust's `std::path` consistently. Test Windows in CI from day one.

3. **Config Schema Validation** (High): TOML + serde with strict mode. Rich error messages with line numbers.

4. **Affected-Target Analysis** (High): Design dependency graph to support "what changed" queries from the start.

5. **Graceful Degradation** (Medium): When hitting limits (memory, watchers), fail with actionable guidance, not panics.

---

## Sources

### Monorepo & Build Systems
- [Solving Monorepo Hell with Bazel](https://medium.com/@erfan.mohebi/solving-monorepo-hell-with-bazel-a-deep-dive-into-modern-build-systems-f70c831bb227)
- [InfoQ - From Monorepo Mess to Monorepo Bliss](https://www.infoq.com/presentations/monorepo-mistakes/)
- [Monorepo Tools Explained](https://monorepo.tools/)
- [Graphite - Managing Dependencies in Monorepo](https://graphite.com/guides/managing-dependencies-monorepo)
- [Graphite - How We Organize Our Monorepo](https://graphite.com/blog/how-we-organize-our-monorepo-to-ship-fast)

### Algorithms & Data Structures
- [Wikipedia - Topological Sorting](https://en.wikipedia.org/wiki/Topological_sorting)
- [Wikipedia - Dependency Graph](https://en.wikipedia.org/wiki/Dependency_graph)
- [NDepend - Detect Dependency Cycles](https://www.ndepend.com/features/dependency-cycles)

### Caching & Consistency
- [Cache Invalidation Strategies](https://softbuilds.medium.com/cache-invalidation-strategies-that-dont-bite-you-later-bde3415687e5)
- [Facebook Engineering - Cache Made Consistent](https://engineering.fb.com/2022/06/08/core-infra/cache-made-consistent/)
- [AlgoMaster - Cache Invalidation](https://algomaster.io/learn/system-design/cache-invalidation)

### Cross-Platform Development
- [Semgrep - Five Considerations for Cross-Platform Tools](https://semgrep.dev/blog/2025/five-considerations-when-building-cross-platform-tools-for-windows-and-macos/)
- [Windows Blog - Symlinks in Windows 10](https://blogs.windows.com/windowsdeveloper/2016/12/02/symlinks-windows-10/)
- [Fixing Git Symlink Issues on Windows](https://sqlpey.com/git/fixing-git-symlink-issues-windows/)

### Configuration Parsing
- [Parsing Config Files The Right Way](https://dmerej.info/blog/post/parsing-config-files-the-right-way/)
- [JSON vs YAML vs TOML - 2026 Comparison](https://dev.to/jsontoall_tools/json-vs-yaml-vs-toml-which-configuration-format-should-you-use-in-2026-1hlb)

### Error Handling & UX
- [NN/g Error Message Guidelines](https://www.nngroup.com/articles/error-message-guidelines/)
- [Rust CLI Book - Error Handling](https://rust-cli.github.io/book/tutorial/errors.html)
- [Effective Error Handling in Rust CLI Apps](https://technorely.com/insights/effective-error-handling-in-rust-cli-apps-best-practices-examples-and-advanced-techniques)

### File System & Concurrency
- [JetBrains - inotify Watches Limit](https://intellij-support.jetbrains.com/hc/en-us/articles/15268113529362-Inotify-Watches-Limit-Linux)
- [GitHub Blog - Improve Git Monorepo Performance with FSMonitor](https://github.blog/engineering/infrastructure/improve-git-monorepo-performance-with-a-file-system-monitor/)
- [Avoiding Race Conditions - Secure Programs HOWTO](https://dwheeler.com/secure-programs/Secure-Programs-HOWTO/avoid-race.html)

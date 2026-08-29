# 02 — DirectoryBackend seam

**What to build:** Introduce the instance handle. Directory-scoped backend calls
(list/reply permissions, list/reply/reject questions, session info) move onto a
`DirectoryBackend` returned by `for_directory(dir)`; the mock implements both
seams; callers that iterate known session directories use the handle instead of
threading a `?directory=` query param. The `Backend` trait stays as the test
seam (ADR-0010) — this is a reshape, not a deletion.

**Blocked by:** None — can start immediately.

**Status:** ready-for-agent

- [ ] No directory-scoped backend method takes a caller-supplied directory; the
      handle owns it.
- [ ] MockBackend implements both the `Backend` and `DirectoryBackend` seams;
      the suite stays green.
- [ ] The silent-omission cwd footgun is gone — omitting a directory is no
      longer representable at the call sites that iterate known directories.
- [ ] Full verification loop green.
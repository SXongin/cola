# Backend seam stays as the test seam; instance routing moves to a DirectoryBackend

cola's OpenCode client is behind a `Backend` trait with two adapters — the real
HTTP `Client` in production and a scripted `MockBackend` in the test harness
(~600 lines). Because two adapters satisfy it, the seam is real, not
hypothetical: keep it. The shallow-looking 1:1 trait methods are the price of
that seam, and a mockable-HTTP replacement would force ~100 tests to be
rewritten for no leverage gain.

The `?directory=` query param is the leak that used to live on those trait
methods: every caller had to remember it, and omitting it silently scoped the
request to the server cwd instance. Instance routing now lives on a
`DirectoryBackend` handle returned by `for_directory(dir)`; directory-scoped
methods (list/reply permissions, list/reply/reject questions, session_info) take
no directory argument. `MockBackend` implements both seams.

## Considered Options

- **Delete the trait; mock the HTTP layer.** Rejected — two adapters (real
  client + mock) already justify the seam, and the rewrite cost outweighs the
  `MockBackend` boilerplate it removes.
- **Keep directory as a caller-remembered parameter.** Rejected — the silent
  cwd-scope footgun is exactly the leak a deep client should own.
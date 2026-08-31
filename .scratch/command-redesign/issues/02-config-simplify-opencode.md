# 02 - Config: `url` optional, drop `username`/`password`

Status: resolved
Type: task
Blocked by: none

## What to build

Simplify `[opencode]` in the config to what is actually needed (ADR-0012):

- `url` becomes `Option<String>` (src/config.rs `OpenCodeConfig`). The effective
  default is `http://localhost:4096` (the port cola's self-started server
  prefers). Discovery still rewrites `cfg.url` when a shared server is found
  (src/main.rs:75-89), so an absent `url` must not break discovery.
- **Delete** `username` and `password` from `OpenCodeConfig`. Discovery already
  supplies the username (`discovery.rs username_from_env`) and password from
  `/proc`; cola's own server strips inherited `OPENCODE_SERVER_USERNAME`
  (pitfall #12). Remove all reads of `cfg.username` / `cfg.password`:
  - `opencode/client.rs` `Client::new` (build_http_client) — always use the
    discovered credentials.
  - `main.rs` lines that set `cfg.username` / `cfg.password`.

## Scope

- `src/config.rs`: `OpenCodeConfig { url: Option<String>, model: Option<String> }`.
- `src/opencode/client.rs`: `Client` always authenticates with the creds passed
  in (discovered); remove the config-driven branches.
- `src/main.rs`: discovery path — if a discovered server has no password (none
  found, self-start), default remains `cola-secret`; drop the username override.
- `README.md` Configuration section + `cola.toml.example`: reflect `url`
  optional, username/password gone.

## Acceptance criteria

- [ ] A config with only `[feishu]` + `[opencode] model` loads and runs;
      discovery attaches to a shared server or self-starts on 4096.
- [ ] `cfg.username` / `cfg.password` no longer exist anywhere.
- [ ] The model three-tier priority is unchanged (`/model` session override >
      `[opencode] model` > server default).
- [ ] `cargo test --workspace --locked` green.
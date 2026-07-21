# deps/

Pinned third-party checkouts used by Cargo `[patch]`.

| path | source | rev |
|------|--------|-----|
| `zed/` | https://github.com/zed-industries/zed | `1a246efd7e1b…` |

`zed/` is **gitignored**. Populate it with:

```bash
./scripts/bootstrap-deps.sh
# or: ln -sfn /path/to/zed-at-that-rev zed
```

See `docs/PLAYBOOK.md`.

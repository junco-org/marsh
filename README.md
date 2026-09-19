# marsh

brush — the bash-compatible shell — running inside a btrfs snapshot, with every command line
instrumented, checked against a capability policy, and published through a write-ahead log, or
discarded.

A marsh shell never writes into the tree it is pointed at. It runs inside one long-lived writable
btrfs snapshot of that tree — the *seed* — and a command line's effects reach the seed only through
a write-ahead log, and only once [junco-policy](https://github.com/junco-org/junco-policy) has
granted every capability they amount to. A crash leaves the seed untouched or completable by a
replay; a refused line leaves it untouched. Alongside, every external command the shell spawns and
every builtin it runs is recorded, which is the only way effects inside the shell process can be
attributed to the command that caused them.

Nothing here forks brush. `MarshExecutor` is a `brush_core::extensions::ExternalCommandSpawner`,
selected statically; `marsh::Shell` wraps a built shell and owns the boundary after every line.
A script running in the shell cannot see the instrumentation, write to it, or turn it off.

## Using it

```sh
cargo build --release
target/release/brush --marsh-seed /srv/seed
```

Without `--marsh-seed` the shell is detached: a stock brush shell, nothing staged, checked or
published.

Every command line is one boundary. Its filesystem effects are diffed against the seed, its git
requests are translated into capability events, and the policy either grants them — the transaction
is logged and applied to the seed — or refuses them, in which case the snapshot is retaken from the
seed and the line's changes are gone:

```
$ target/debug/brush --marsh-seed "$SEED" --norc --noprofile -s
printf hi > p.txt
git add -- p.txt
git add -- p.txt
marsh: 1 of 1 capabilities denied; the line's changes were discarded
  - 23730ac9 stage p.txt: stage requires an unstaged resource owned by the acting principal
    fix: 23730ac9 unstage p.txt before staging again
```

State lives under `$HOME/.marsh/<seed-name>/`: `snap/` holds the session's snapshot, `meta/wal.jsonl`
the write-ahead log, `meta/runs/<uid>/` the spawn and builtin record streams.

## As a library

```rust
let validator = marsh::PolicyValidator::global();
let shell = marsh::Shell::new(std::path::Path::new("/srv/seed"), validator).await?;
let (result, outcome) = shell.run("printf hi > greeting").await?;
```

`marsh::Shell::run` and `marsh::Shell::conclude` are the boundary; `marsh::Outcome` says whether the
line was published, denied — with the policy's explanations — or ran detached.

## Layout

| Path | What it is |
| --- | --- |
| `src/lib.rs` | the `marsh` facade crate: the gated shell, its executor and its policy |
| `crates/marsh-btrfs` | seed discovery, the session lease, snapshots |
| `crates/marsh-wal` | tree diffing and durable publication |
| `crates/marsh-instrument` | the builtin hook and the record vocabulary |
| `crates/brush-shell` | upstream brush-shell, vendored, plus `src/marsh/` and the `brush` binary |

`crates/brush-shell` and `fuzz` are vendored from [the brush fork](https://github.com/elefthei/brush)
and keep their upstream names; every brush crate is redirected there by the root manifest's
`[patch.crates-io]`.

## Requirements

Linux, btrfs for the seed (a non-root user needs `user_subvol_rm_allowed` on it), and the Rust
toolchain named by `rust-version` in the root manifest.

## Development

```sh
cargo build --workspace --all-targets
cargo clippy --workspace --all-targets
cargo doc --workspace --no-deps
cargo nextest run --workspace
cargo nextest run -p brush-shell -E 'binary(marsh)'   # marsh's own tests
```

The workspace denies warnings, `rustdoc::all`, and clippy `pedantic`/`nursery`/`cargo`.

## License

MIT. See [LICENSE](LICENSE).

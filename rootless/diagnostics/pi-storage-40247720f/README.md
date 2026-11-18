# Pi Workspace history disk usage

Historical diagnostic for product commit `40247720f9df0c74cbb0769cbe6b39632bb9c098`.
Pi coding agent 1.5.2 drove an x86-64 UML guest with a Disposable Store on a
Linux ext4 host. Each case retained the initial import and eleven explicit Pi
Workspace commits. All twelve revisions reopened on an independent disk copy.
The verifier checked a changed file in each revision and the final source
tree's file names and contents.

| Twelve retained states | Full copies (MiB) | Hard-linked copies (MiB) | Project image (MiB) | Saved vs full copies |
| --- | ---: | ---: | ---: | ---: |
| Source changes | 152.36 | 21.42 | 20.74 | 86.4% |
| 4 KiB patches to a 4 MiB binary | 48.05 | 48.05 | 21.67 | 54.9% |
| Whole 4 MiB binary replacements | 48.05 | 48.05 | 66.04 | −37.4% |

The source case replayed the last twelve first-parent revisions changing
`core/crates` through the pinned product commit. Both binary cases started
with dense SplitMix64 output at seed zero. Revision *i* used seed *i* for
either the replacement file or a 4 KiB patch at block 73 × *i*. Full copies
retained an independent tree per state. Hard-linked copies shared regular
files with the same path, contents and mode as in the preceding state.

The table counts physical allocation after clean close. GNU `du -s -B1`
measured each baseline, including directories. The project image measurement
used `st_blocks × 512`. After the commits, each case held a fresh, unmodified
Pi Workspace open for ten seconds and discarded it. All eight ext4 inode
tables had been zeroed by then and occupied 16 MiB in each image. In the
patch case the image grew from 5.60 to 21.67 MiB during this observation,
with unchanged Store size and history. The first source idle verification
compared commit lists in opposite orders and failed. Correcting the order
validated the existing observation without repeating the idle session.

Savings are `1 − image / baseline`. Each project shares the 84.81 MiB runtime
(excluding the optional source bundle). Counting the runtime and the retained
12.66 MiB import directory against the source case alone brings its total to
118.21 MiB, a 22.4% saving versus full copies. These figures describe one
trajectory per workload.

[Per-state bytes and witnesses](checkpoints.csv) · [Final byte counts and percentages](summary.json) · [Runtime hashes](runtime.sha256).
Each case directory holds the observed history and the before/after idle allocation.

## Reproduce

Use a rootless bundle built from the pinned product, Pi 1.5.2 on PATH, a C
compiler, Python 3, GNU utilities and e2fsprogs. Run from the checkout root
on an ext4 host with room for the baseline trees and sparse project images.
Each run uses a fresh output directory. The commands below use the same
workloads and call bounds as the recorded run.

```sh
export BUNDLE=/absolute/path/to/rootless-bundle
export RUN_DIR=$(mktemp -d)
export LAYERFS_CONSTRUCTION_WORKERS=1
diag=rootless/diagnostics/pi-storage-40247720f
cc -O2 -s "$diag/generate.c" -o "$RUN_DIR/generate"
for case in source patch replace; do
    timeout 100 python3 "$diag/storage.py" "$case" || exit 1
done
timeout 52 python3 "$diag/idle.py"
```

The driver writes a checkpoint after every commit, checks historical reads
on a copy, and records the idle results separately. Compare raw allocation
on the same filesystem. Sparse allocation and the timing of ext4 inode
table initialization can vary between runs.

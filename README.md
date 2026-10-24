# onemodel

Every local AI app keeps its own copy of the model weights. Pull a GGUF into Ollama, download the same file in LM Studio, let llama.cpp fetch it with `-hf`, and one model now takes up its size several times over. GGUF tools can share a folder, but Ollama stores weights as content-addressed blobs and cannot point at another app's files. `onemodel` finds files with identical bytes across these stores and replaces the extra copies with copy-on-write clones of one copy. Every app still sees its own file at its own path, the disk space comes back, and `onemodel undo` turns the clones back into independent copies.

## Install

You need a Rust toolchain (1.85 or newer).

```sh
cargo install --path crates/onemodel
```

This puts the `onemodel` binary in `~/.cargo/bin`. It runs on macOS with APFS. The Linux code path (FICLONE on btrfs or XFS) is written but untested.

## Use it

Start with `scan`. It reads the stores and changes nothing.

```console
$ onemodel scan
stores
  ollama       ~/.ollama/models              2 files    2.20 GB
  huggingface  ~/.cache/huggingface/hub      3 files   491.3 MB
  lmstudio     ~/.lmstudio/models             1 file   270.6 MB
  llama.cpp    ~/Library/Caches/llama.cpp     1 file   270.6 MB
duplicates
   270.6 MB  6cb0ba7f  keep    ~/.cache/huggingface/hub/blobs/47/4788feabb162…
                       clone   ~/.lmstudio/models/bartowski/SmolLM2-360M-Instruct-GGUF/SmolLM2-360M-Instruct-Q4_K_M.gguf
                       clone   ~/.ollama/models/blobs/sha256-2fa3f013dcdd…
                       clone   ~/Library/Caches/llama.cpp/models--bartowski--SmolLM2-360M-Instruct-GGUF/blobs/2fa3f013dcdd…
reclaimable 811.8 MB (3 to clone)
```

Then replace the duplicates. `dedupe --dry-run` prints the plan without changing anything.

```console
$ onemodel dedupe
replaced  ~/.lmstudio/models/bartowski/SmolLM2-360M-Instruct-GGUF/SmolLM2-360M-Instruct-Q4_K_M.gguf  (clone of ~/.cache/huggingface/hub/blobs/47/4788feabb162…)
replaced  ~/.ollama/models/blobs/sha256-2fa3f013dcdd…  (clone of ~/.cache/huggingface/hub/blobs/47/4788feabb162…)
replaced  ~/Library/Caches/llama.cpp/models--bartowski--SmolLM2-360M-Instruct-GGUF/blobs/2fa3f013dcdd…  (clone of ~/.cache/huggingface/hub/blobs/47/4788feabb162…)
freed 810.4 MB on /System/Volumes/Data (free 151.44 GB -> 152.25 GB)

$ onemodel verify
ok: 4 managed files (1.08 GB) hashed, 23 store claims checked in 4 stores

$ onemodel undo
restored  ~/Library/Caches/llama.cpp/models--bartowski--SmolLM2-360M-Instruct-GGUF/blobs/2fa3f013dcdd…
restored  ~/.ollama/models/blobs/sha256-2fa3f013dcdd…
restored  ~/.lmstudio/models/bartowski/SmolLM2-360M-Instruct-GGUF/SmolLM2-360M-Instruct-Q4_K_M.gguf
used 813.5 MB on /System/Volumes/Data (free 152.25 GB -> 151.43 GB)
```

| Command | What it does |
| --- | --- |
| `onemodel scan` | Lists the stores it found, the duplicate groups, and the space a dedupe would free. |
| `onemodel dedupe` | Replaces duplicates with clones. Resumes an interrupted run first. `--dry-run` prints the plan only. |
| `onemodel verify` | Hashes every file onemodel manages and checks each store's own integrity. Exits 2 on any problem. `--deep` also hashes every Ollama blob against its manifest. |
| `onemodel undo` | Makes every replaced file an independent copy again. `--only <PATH>` limits it to files under a path, and `--run <ID>` to one run. |

Options that work with every command:

- `--path <DIR>` adds a directory as a store. Repeat it for more directories.
- `--min-size <SIZE>` ignores smaller files. The default is `32M`.
- `--state <DIR>` (or `ONEMODEL_STATE`) moves the journal. The default is `~/Library/Application Support/onemodel` on macOS.
- `--json` prints one JSON document instead of text.

`onemodel` finds these stores by itself:

| App | Where it looks |
| --- | --- |
| Ollama | `$OLLAMA_MODELS`, else `~/.ollama/models` |
| Hugging Face | `$HF_HUB_CACHE`, else `$HF_HOME/hub`, else `~/.cache/huggingface/hub` |
| LM Studio | `~/.lmstudio/models`, `~/.cache/lm-studio/models`, and the `downloadsFolder` in `~/.lmstudio/settings.json` |
| llama.cpp | `$LLAMA_CACHE`, else `~/Library/Caches/llama.cpp` (macOS) or `~/.cache/llama.cpp` (Linux) |

A new store is one file in `crates/onemodel/src/stores/` plus one line in the registry in `stores/mod.rs`.

## Measured results

All numbers come from one MacBook (Apple silicon, APFS, warm page cache), measured on 2026-10-24. Freed space is the change in free space that `statfs` reports, because APFS clones do not show up in `du`.

| Setup | Stores | Duplicates found | Reclaimed | `scan` | `dedupe` | `undo` |
| --- | --- | --- | --- | --- | --- | --- |
| SmolLM2-360M Q4_K_M (270.6 MB) installed by Ollama, Hugging Face, LM Studio and llama.cpp | 4 | 3 | 0.81 GB | 0.43 s | 1.11 s | 0.82 s |
| Qwen2.5-3B Q4_K_M (1.93 GB), two copies in two `--path` folders | 2 | 1 | 1.93 GB | 2.47 s | 3.57 s | 1.75 s |

After the first row's dedupe, `ollama run` on both Ollama model names and `llama-completion` on the LM Studio, Hugging Face and llama.cpp paths each loaded the shared file and answered "Paris" to "The capital of France is", with temperature 0. They gave the same answer again after `undo`.

## Which apps really store the same bytes

`onemodel` only helps when two apps hold byte-identical files. On this Mac:

- `ollama pull hf.co/<user>/<repo>:<quant>` and `ollama create` from a local GGUF both store the GGUF unchanged. The blob name is the file's SHA-256, the same value Hugging Face lists for the file. These match LM Studio, llama.cpp and Hugging Face downloads of the same file.
- Ollama library models (`ollama pull qwen2.5:3b`) are packaged by Ollama. The `qwen2.5:3b` weights blob matched neither the official Qwen GGUF nor the bartowski GGUF of the same quantization. That is one sample, but do not count on library models matching anything.
- llama.cpp b11850 downloads `-hf` models into the Hugging Face cache, so by default it already shares files with Hugging Face. With `LLAMA_CACHE` set, it writes a separate copy in the same layout, and `onemodel` dedupes that copy.

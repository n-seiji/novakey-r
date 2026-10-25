#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.12.*"
# dependencies = [
#   "huggingface_hub",
#   "safetensors",
#   "numpy",
#   "sentencepiece",
#   "protobuf",
#   "tokenizers",
#   "transformers",
# ]
#
# [tool.uv]
# # Supply-chain guard: refuse package versions published less than 7 days ago.
# exclude-newer = "7 days"
# ///
"""Build the on-device judge model files in resources/judge/ (they are not in git).

  resources/judge/model_q8_0.gguf   rinna/japanese-gpt2-xsmall as q8_0 blocks (src/judge.rs reads this)
  resources/judge/tokenizer.json    the SentencePiece model converted to a `tokenizers` file

Source: https://huggingface.co/rinna/japanese-gpt2-xsmall (MIT), pinned to REVISION.

Layout of the gguf (what src/judge.rs expects):
  - 2-D weights except wpe / wte are transposed to [out, in] and quantized to q8_0
  - transformer.wte.weight ([vocab, d_model]) is quantized to q8_0 as is
  - everything else (wpe, biases, layer norms) stays f32
  - the causal-mask buffers (attn.bias / attn.masked_bias) are dropped

Usage:  mise run judge-model        (or: uv run --locked --script tools/gen_judge_model.py [--out DIR])
        tools/gen_judge_model.py --verify-only [--out DIR]   check the files against tools/judge.sha256
Set HF_HUB_OFFLINE=1 to use only the local Hugging Face cache; HF_TOKEN is used if set.

The files are checked against tools/judge.sha256 after they are written. The dependencies are
locked in tools/gen_judge_model.py.lock (uv lock --script tools/gen_judge_model.py).
"""

import argparse
import hashlib
import os
import struct
import sys
import time
from pathlib import Path

import numpy as np

REPO = "rinna/japanese-gpt2-xsmall"
REVISION = "8e91527b3276e0565154935e84a08bf0137ed99f"

GGUF_MAGIC = b"GGUF"
GGUF_VERSION = 2
GGML_F32 = 0
GGML_Q8_0 = 8
ALIGNMENT = 32
BLOCK = 32

HASH_FILE = Path(__file__).resolve().parent / "judge.sha256"
FETCH_ATTEMPTS = 3  # the first try plus two retries


def quantize_q8_0(x: np.ndarray) -> bytes:
    """ggml q8_0: per block of 32 floats, f16 scale d = amax / 127, then 32 i8 = round(x / d)."""
    x = np.ascontiguousarray(x, dtype=np.float32).reshape(-1, BLOCK)
    amax = np.abs(x).max(axis=1)
    d = amax / np.float32(127.0)
    inv = np.divide(np.float32(1.0), d, out=np.zeros_like(d), where=d != 0)
    scaled = x * inv[:, None]
    # f32::round in Rust rounds half away from zero (numpy's round is half-to-even)
    q = (np.sign(scaled) * np.floor(np.abs(scaled) + np.float32(0.5))).astype(np.int8)
    out = np.empty((x.shape[0], 34), dtype=np.uint8)
    out[:, :2] = d.astype(np.float16).view(np.uint8).reshape(-1, 2)
    out[:, 2:] = q.view(np.uint8)
    return out.tobytes()


def gguf_string(s: str) -> bytes:
    b = s.encode()
    return struct.pack("<Q", len(b)) + b


def write_gguf(path: Path, tensors: list[tuple[str, tuple[int, ...], int, bytes]]) -> None:
    """tensors: (name, shape in row-major order, ggml dtype, raw bytes)."""
    infos = b""
    offset = 0
    pads = []
    for name, shape, dtype, data in tensors:
        infos += gguf_string(name) + struct.pack("<I", len(shape))
        infos += b"".join(struct.pack("<Q", n) for n in reversed(shape))  # ggml lists the fastest dim first
        infos += struct.pack("<IQ", dtype, offset)
        pad = -len(data) % ALIGNMENT
        pads.append(pad)
        offset += len(data) + pad
    header = GGUF_MAGIC + struct.pack("<IQQ", GGUF_VERSION, len(tensors), 0) + infos
    header += b"\0" * (-len(header) % ALIGNMENT)
    with open(path, "wb") as f:
        f.write(header)
        for (_, _, _, data), pad in zip(tensors, pads):
            f.write(data + b"\0" * pad)


def build_gguf(safetensors_path: str, out: Path) -> None:
    from safetensors.numpy import load_file

    weights = load_file(safetensors_path)
    tensors = []
    for name in sorted(weights):
        if name.endswith(".attn.bias") or name.endswith("masked_bias"):
            continue
        w = weights[name].astype(np.float32)
        if w.ndim == 2 and name != "transformer.wpe.weight":
            if name != "transformer.wte.weight":
                w = np.ascontiguousarray(w.T)  # Conv1D stores [in, out]
            if w.shape[1] % BLOCK:
                raise SystemExit(f"{name}: {w.shape[1]} columns is not a multiple of {BLOCK}")
            tensors.append((name, w.shape, GGML_Q8_0, quantize_q8_0(w)))
        else:
            tensors.append((name, w.shape, GGML_F32, np.ascontiguousarray(w).tobytes()))
    write_gguf(out, tensors)


def build_tokenizer(spiece_path: str, out: Path) -> None:
    import sentencepiece as spm
    from transformers.convert_slow_tokenizer import SpmConverter

    class Slow:
        vocab_file = spiece_path
        additional_special_tokens: list = []

        def __init__(self) -> None:
            self.sp_model = spm.SentencePieceProcessor(model_file=spiece_path)

    tok = SpmConverter(Slow()).converted()
    tok.save(str(out))

    # The converted tokenizer must agree with SentencePiece itself.
    sp = spm.SentencePieceProcessor(model_file=spiece_path)
    for text in ["今日は天気がいい", "きょうのゆうごはんはなににしよう", "mergeして", "ごみを捨てないでください"]:
        a, b = sp.encode(text.lower()), tok.encode(text.lower(), add_special_tokens=False).ids
        if a != b:
            raise SystemExit(f"tokenizer mismatch on a probe string: {a} != {b}")


def fetch(filename: str) -> str:
    """Download one file of REPO at REVISION, retrying twice. HF_TOKEN is used when set."""
    from huggingface_hub import hf_hub_download

    token = os.environ.get("HF_TOKEN") or None
    for attempt in range(1, FETCH_ATTEMPTS + 1):
        try:
            return hf_hub_download(REPO, filename, revision=REVISION, token=token)
        except Exception as e:  # network and hub errors vary; the last one is re-raised
            if attempt == FETCH_ATTEMPTS:
                raise
            print(f"fetching {filename} failed ({type(e).__name__}); retrying", file=sys.stderr)
            time.sleep(2 * attempt)
    raise AssertionError("unreachable")


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def verify(out: Path) -> None:
    """Check <out>/<name> against each `<sha256>  judge/<name>` line of tools/judge.sha256."""
    lines = [ln.split() for ln in HASH_FILE.read_text().splitlines() if ln.strip()]
    if not lines:
        raise SystemExit(f"{HASH_FILE} has no entries")
    for expected, rel in lines:
        name = rel.removeprefix("judge/")
        path = out / name
        if not path.is_file() or sha256_file(path) != expected:
            raise SystemExit(f"judge model hash mismatch: {name}")
    print(f"verified {out} against {HASH_FILE.name}")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", type=Path, default=Path(__file__).resolve().parent.parent / "resources" / "judge")
    ap.add_argument("--verify-only", action="store_true", help="only check the existing files against tools/judge.sha256")
    args = ap.parse_args()
    if args.verify_only:
        verify(args.out)
        return
    args.out.mkdir(parents=True, exist_ok=True)

    weights = fetch("model.safetensors")
    spiece = fetch("spiece.model")
    build_gguf(weights, args.out / "model_q8_0.gguf")
    build_tokenizer(spiece, args.out / "tokenizer.json")
    print(f"wrote {args.out}/model_q8_0.gguf and tokenizer.json")
    verify(args.out)


if __name__ == "__main__":
    main()

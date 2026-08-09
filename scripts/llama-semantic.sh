#!/usr/bin/env bash
# Start/stop/status the two local llama.cpp servers `repo`'s semantic search uses.
#
#   embedder  :8081  ggml-org/bge-small-en-v1.5-Q8_0-GGUF   (--embedding)
#   reranker  :8082  gpustack/bge-reranker-v2-m3-GGUF:Q8_0   (--reranking --pooling rank)
#
# Both need --ubatch-size 2048: llama.cpp's default 512 overflows on real code
# chunks (embed) and long query+doc pairs (rerank). bge-small also has a hard
# 512-token model context, which `repo` handles with adaptive truncation.
#
# Usage: scripts/llama-semantic.sh {start|stop|status|restart}
set -euo pipefail

HF="$HOME/.cache/huggingface/hub"
EMBED_PORT=8081
RERANK_PORT=8082
EMBED_LOG=/tmp/repo-embedder.log
RERANK_LOG=/tmp/repo-reranker.log

resolve() { find "$HF" -path "*/$1" -type f 2>/dev/null | head -1; } # snapshots/<hash>/file.gguf

EMBED_MODEL="$(resolve "models--ggml-org--bge-small-en-v1.5-Q8_0-GGUF/snapshots/*/bge-small-en-v1.5-q8_0.gguf")"
RERANK_MODEL="$(resolve "models--gpustack--bge-reranker-v2-m3-GGUF/snapshots/*/bge-reranker-v2-m3-Q8_0.gguf")"

start() {
  : "${EMBED_MODEL:?bge-small GGUF not found — run: hf download ggml-org/bge-small-en-v1.5-Q8_0-GGUF bge-small-en-v1.5-q8_0.gguf}"
  : "${RERANK_MODEL:?bge-reranker GGUF not found — run: hf download gpustack/bge-reranker-v2-m3-GGUF bge-reranker-v2-m3-Q8_0.gguf}"
  nohup llama serve -m "$EMBED_MODEL" --embedding --host 127.0.0.1 --port "$EMBED_PORT" \
    --ctx-size 2048 --batch-size 2048 --ubatch-size 2048 > "$EMBED_LOG" 2>&1 &
  echo "embedder → :$EMBED_PORT  (pid $!, log $EMBED_LOG)"
  nohup llama serve -m "$RERANK_MODEL" --reranking --embedding --pooling rank --host 127.0.0.1 --port "$RERANK_PORT" \
    --ctx-size 4096 --batch-size 2048 --ubatch-size 2048 > "$RERANK_LOG" 2>&1 &
  echo "reranker → :$RERANK_PORT  (pid $!, log $RERANK_LOG)"
  echo "waiting for models to load…"
  for p in "$EMBED_PORT" "$RERANK_PORT"; do
    for _ in $(seq 1 60); do
      curl -sf -m1 "http://127.0.0.1:$p/health" >/dev/null 2>&1 && break
      sleep 1
    done
  done
  status
}

stop() {
  pkill -f 'bge-small-en-v1.5-q8_0' 2>/dev/null || true
  pkill -f 'bge-reranker-v2-m3-Q8_0' 2>/dev/null || true
  echo "stopped"
}

status() {
  for spec in "embedder:$EMBED_PORT" "reranker:$RERANK_PORT"; do
    name="${spec%%:*}"; port="${spec##*:}"
    if curl -sf -m2 "http://127.0.0.1:$port/health" >/dev/null 2>&1; then
      echo "  $name  :$port  UP"
    else
      echo "  $name  :$port  DOWN"
    fi
  done
}

case "${1:-status}" in
  start) start ;;
  stop) stop ;;
  restart) stop; start ;;
  status) status ;;
  *) echo "usage: $0 {start|stop|status|restart}" >&2; exit 1 ;;
esac

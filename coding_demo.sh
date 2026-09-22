#!/usr/bin/env bash
# Drive a small coding task entirely through the LOCAL youdao proxy
# (the vllm/sglang-compatible OpenAI interface on 127.0.0.1:8080).
# This proves a coding agent can be powered by the local service:
#  1. ask the local LLM to write code
#  2. save the code it returns
#  3. actually run it and show the result
set -euo pipefail

BASE="http://127.0.0.1:8080/v1"
MODEL="deepseek-r1"
OUT="/tmp/youdao_coding_out"
mkdir -p "$OUT"
echo "=================================================="
echo " coding demo — powered by LOCAL youdao proxy ($BASE)"
echo "=================================================="

PROMPT='用 Python 写一个带记忆化的 Fibonacci 函数 fib(n)（迭代或递归+缓存均可），'
PROMPT+='并写一段 __main__ 自检：计算 fib(10)、fib(30) 并打印结果，最后打印 "SELF-TEST OK"。'
PROMPT+=' 只输出一个 python 代码块，不要任何解释。'

# Build the JSON body safely with python (avoids quoting/backtick pitfalls).
REQ=$(python3 - "$MODEL" "$PROMPT" <<'PY'
import sys, json
model, prompt = sys.argv[1], sys.argv[2]
print(json.dumps({"model": model,
                  "messages": [{"role": "user", "content": prompt}],
                  "max_tokens": 1024, "temperature": 0}, ensure_ascii=False))
PY
)

echo "[$(date +%H:%M:%S)] -> /v1/chat/completions (model=$MODEL)"
RAW=$(curl -s --max-time 90 "${BASE}/chat/completions" \
      -H 'Content-Type: application/json' -d "${REQ}")

echo "raw usage: $(echo "$RAW" | python3 -c "import sys,json;print(json.load(sys.stdin).get('usage',{}))" 2>/dev/null || echo '<parse-failed>')"

# Pull the code out of the (first) python block in content, else use raw content.
CODE=$(echo "$RAW" | python3 -c '
import sys,json,re
m=json.load(sys.stdin)
c=m["choices"][0]["message"]["content"]
b=re.search(r"```python\s*(.*?)```", c, re.S)
sys.stdout.write((b.group(1) if b else c).strip()+"\n")
')
echo "[$(date +%H:%M:%S)] generated code saved to ${OUT}/fib.py"
printf '%s\n' "$CODE" > "${OUT}/fib.py"
echo "---- generated code ----"
cat "${OUT}/fib.py"
echo "------------------------"

echo "[$(date +%H:%M:%S)] running the generated program..."
python3 "${OUT}/fib.py"

echo "=================================================="
echo " done — code was written AND executed via the local service"
echo "=================================================="

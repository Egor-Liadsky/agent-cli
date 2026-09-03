#!/usr/bin/env python3
"""Решает одну задачу четырьмя способами промптинга и сохраняет ответы.

Backend по умолчанию — CLI `claude -p` (не требует отдельного ключа).
С ключом ANTHROPIC_API_KEY можно переключиться на HTTP API: --backend api
"""
import argparse
import json
import os
import subprocess
import sys
import urllib.request
from pathlib import Path

HERE = Path(__file__).parent
TASK = """В ящике 5 монет: 2 монеты с орлом на обеих сторонах, 1 монета с решкой на обеих сторонах, 2 честные монеты. \
Наугад берут одну монету и подбрасывают её два раза. Оба раза выпал орёл. \
Какова вероятность того, что взятая монета — двуорловая?"""

MODEL = "claude-sonnet-5"


def ask_cli(prompt: str, model: str) -> str:
    out = subprocess.run(
        ["claude", "-p", "--model", model, prompt],
        capture_output=True, text=True, timeout=300,
    )
    if out.returncode != 0:
        raise RuntimeError(out.stderr.strip()[:500])
    return out.stdout.strip()


def ask_api(prompt: str, model: str) -> str:
    key = os.environ["ANTHROPIC_API_KEY"]
    base = os.environ.get("ANTHROPIC_BASE_URL", "https://api.anthropic.com").rstrip("/")
    body = json.dumps({
        "model": model,
        "max_tokens": 2000,
        "messages": [{"role": "user", "content": prompt}],
    }).encode()
    req = urllib.request.Request(
        f"{base}/v1/messages",
        data=body,
        headers={
            "content-type": "application/json",
            "x-api-key": key,
            "anthropic-version": "2023-06-01",
        },
    )
    with urllib.request.urlopen(req, timeout=300) as resp:
        data = json.load(resp)
    return "".join(b.get("text", "") for b in data["content"]).strip()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--backend", choices=["cli", "api"], default="cli")
    ap.add_argument("--model", default=MODEL)
    args = ap.parse_args()
    ask = ask_cli if args.backend == "cli" else ask_api

    results: dict[str, str] = {}

    # Способ 1: прямой вопрос без инструкций.
    p1 = TASK + "\n\nДай ответ."
    results["1_direct"] = ask(p1, args.model)

    # Способ 2: пошаговое рассуждение.
    p2 = TASK + "\n\nРешай пошагово: выпиши все гипотезы, их априорные вероятности, " \
                "правдоподобия и примени формулу Байеса. В конце дай итоговое число."
    results["2_step_by_step"] = ask(p2, args.model)

    # Способ 3: модель сама пишет промпт, затем решает по нему.
    meta = (
        "Ниже задача. Не решай её. Составь оптимальный промпт для языковой модели, "
        "который максимизирует шанс получить строго верный ответ. Верни только текст промпта.\n\n"
        f"Задача: {TASK}"
    )
    generated = ask(meta, args.model)
    results["3a_generated_prompt"] = generated
    results["3b_answer_by_generated"] = ask(generated, args.model)

    # Способ 4: группа экспертов.
    p4 = (
        "Ты модерируешь группу из трёх экспертов, которые решают задачу.\n"
        "- Аналитик: формализует пространство событий и априорные вероятности.\n"
        "- Инженер: считает по формуле Байеса, показывает арифметику дробями.\n"
        "- Критик: ищет ошибки в рассуждениях коллег (особенно подмену одного броска двумя "
        "и путаницу P(данные) с P(гипотеза|данные)).\n\n"
        "Дай отдельный разбор от каждого эксперта, затем согласованный финальный ответ одним числом.\n\n"
        f"Задача: {TASK}"
    )
    results["4_experts"] = ask(p4, args.model)

    (HERE / "results.json").write_text(json.dumps(results, ensure_ascii=False, indent=2))
    md = ["# Ответы модели (%s, backend=%s)\n" % (args.model, args.backend)]
    for k, v in results.items():
        md.append(f"\n## {k}\n\n{v}\n")
    (HERE / "results.md").write_text("".join(md))
    print("saved results.md / results.json")
    return 0


if __name__ == "__main__":
    sys.exit(main())

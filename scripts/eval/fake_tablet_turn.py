# /// script
# requires-python = ">=3.10"
# dependencies = ["pillow>=10"]
# ///
"""One full selection turn with no tablet: a local stand-in for the Paper Pro client.

    uv run scripts/eval/fake_tablet_turn.py              # stubbed model (default, labelled)
    uv run scripts/eval/fake_tablet_turn.py --live claude # real Claude Code CLI login (full tool access!)

What is real and what is a stand-in:

* REAL: bridge/codex_bridge.py (HTTP server, auth, request validation, receipts,
  the no-replay retry path, answer_instructions, run_codex's CLI command,
  format_answer_sources, validate_answer) and bridge/backend_router.py.
* REAL inputs: prompts/selection_concise_ink.json and prompts/tool_draw_answer.json,
  and the repo's own handwriting fixture evaluations/blank_math/input.png.
* STAND-IN (device): the request is assembled the way src/coordinator.rs:430-595
  and src/llm_engine/openai.rs build it; placement mirrors src/page_layout.rs
  (ink_mask/rect_after) and pagination mirrors src/answer_delivery.rs (wrap_lines,
  capacity, deliver). Previews are approximations of the pen output, drawn with
  the same IBM Plex Mono size and pitch; the illustration geometry follows
  src/illustration.rs:svg.
* STUB (model): scripts/eval/fake_codex.py answers with scripts/eval/stub_answer.json.

Nothing here opens SSH, touches /dev/input, or talks to a reMarkable.
"""
import argparse
import base64
import json
import os
import secrets
import shutil
import sys
import tempfile
import textwrap
import threading
import urllib.request
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "bridge"))
import codex_bridge  # noqa: E402  (the real bridge)

W, H, PAGE_BOTTOM = 768, 1024, 980  # virtual screen, src/page_layout.rs:7-10


# --- device stand-in: placement (mirror of src/page_layout.rs) ------------------
def ink_bottom(page):
    """Lowest 'ink' row in virtual coordinates (ink_mask + bottom, non-Move, no palette)."""
    small = page.convert("RGB").resize((W, H))
    px = small.load()
    for y in range(PAGE_BOTTOM - 1, 54, -1):
        for x in range(10, W - 10):
            r, g, b = px[x, y]
            lo, hi = min(r, g, b), max(r, g, b)
            if hi < 220 or (hi - lo > 50 and lo < 220):
                return y
    return None


def rect_after(last, x):
    y = max(last, 55) + 16
    h = PAGE_BOTTOM - 10 - y
    assert h >= 96, "No clear space below the page's writing"
    x = min(max(x, 64), W - 310)
    return {"x": x, "y": y, "w": W - 10 - x, "h": h}


# --- device stand-in: pagination (mirror of src/answer_delivery.rs) -------------
def wrap_lines(lines, width):
    out = []
    for text in lines:
        line = ""
        for word in text.split():
            if line and len(line) + 1 + len(word) > width:
                out.append(line)
                line = ""
            for i in range(0, len(word), width):
                part = word[i:i + width]
                line = (line + " " + part) if line else part
                if len(line) >= width:
                    out.append(line)
                    line = ""
        if line:
            out.append(line)
    assert out and len(out) <= 256
    return out


def plan_pages(lines, drawings, rect):
    width = max(12, min(52, int((rect["w"] - 28) / 13.2)))
    wrapped = wrap_lines(lines, width)
    cursor, figure, pages, plan, required = 0, 0, [], None, rect["w"]
    page = {"page": "original (after the question)", "areas": []}
    while True:
        cap = (rect["h"] - 34) // 31
        end = min(cursor + cap, len(wrapped))
        if cursor < len(wrapped):
            area = {"rect": dict(rect), "lines": wrapped[cursor:end]}
            used = 34 + (end - cursor) * 31
            cursor = end
        else:
            area = {"rect": dict(rect), "illustration": drawings[figure]["title"], "figure": figure}
            figure += 1
            used = 440
        more = cursor < len(wrapped) or figure < len(drawings)
        area["status"] = "continued (arrow)" if more else "complete (check)"
        page["areas"].append(area)
        if not more:
            pages.append(page)
            return {"wrap_width": width, "wrapped_line_count": len(wrapped), "pages": pages}
        if cursor == len(wrapped) and rect["w"] >= 628 and rect["h"] - used - 18 >= 440:
            rect = {**rect, "y": rect["y"] + used + 18, "h": rect["h"] - used - 18}
            continue
        assert len(pages) + 1 < 12, "Answer continuation limit reached"
        pages.append(page)
        # A native note page inserted after the current one: blank canvas, scan from top.
        rect = rect_after(55, 64 if cursor == len(wrapped) else rect["x"])
        assert rect["w"] >= required, "Continuation page is narrower than the answer"
        page = {"page": f"inserted note page {len(pages)}", "areas": []}


# --- preview rendering (approximate pen output) ----------------------------------
def render(plan, base, colour=(30, 60, 200)):
    font = ImageFont.truetype(str(REPO / "assets/fonts/IBMPlexMono-Regular.ttf"), 22)
    small = ImageFont.truetype(str(REPO / "assets/fonts/IBMPlexMono-Regular.ttf"), 14)
    title = ImageFont.truetype(str(REPO / "assets/fonts/IBMPlexMono-Regular.ttf"), 20)
    label = ImageFont.truetype(str(REPO / "assets/fonts/IBMPlexMono-Regular.ttf"), 18)
    images = []
    for index, page in enumerate(plan["pages"]):
        img = (base.convert("RGB").resize((W, H)) if index == 0 else Image.new("RGB", (W, H), "white"))
        d = ImageDraw.Draw(img)
        for area in page["areas"]:
            r = area["rect"]
            x, y = r["x"] + 6, r["y"] + 4
            d.rectangle([x, y, x + 12, y + 12], outline=colour, width=2)       # status box
            mark = area["status"]
            if mark.startswith("complete"):
                d.line([(x + 2, y + 6), (x + 5, y + 9), (x + 10, y + 2)], fill=colour, width=2)
            else:
                d.line([(x + 2, y + 6), (x + 10, y + 6)], fill=colour, width=2)
                d.line([(x + 6, y + 2), (x + 10, y + 6), (x + 6, y + 10)], fill=colour, width=2)
            d.text((x + 20, y + 13), "AI codex / default", font=small, fill=colour, anchor="ls")
            if "lines" in area:
                top = r["y"] + 28
                for i, line in enumerate(area["lines"]):
                    d.text((r["x"] + 22, top + 22 + i * 31), line, font=font, fill=colour, anchor="ls")
                bottom = top + len(area["lines"]) * 31 + 3
                d.line([(x + 7, top), (x, top), (x, bottom), (x + 7, bottom)], fill=colour, width=2)
            else:
                drawing = DRAWINGS[area["figure"]]
                d.line([(r["x"] + 13, r["y"] + 28), (r["x"] + 6, r["y"] + 28), (r["x"] + 6, r["y"] + 430),
                        (r["x"] + 13, r["y"] + 430)], fill=colour, width=2)
                d.text((r["x"] + 22, r["y"] + 47), drawing["title"], font=title, fill=colour, anchor="ls")
                ox, oy = r["x"] + 22, r["y"] + 64
                for stroke in drawing["strokes"]:
                    d.line([(ox + p["x"], oy + p["y"]) for p in stroke], fill=colour, width=2)
                for lab in drawing["labels"]:
                    d.text((ox + lab["x"], oy + lab["y"]), lab["text"], font=label, fill=colour, anchor="ls")
        images.append(img)
    return images


DRAWINGS = []


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--live", choices=["claude", "codex"], help="call a real CLI instead of the stub")
    parser.add_argument("--out", type=Path, default=REPO / "docs/eval/mattpetters-turn")
    parser.add_argument("--reply-length", default="detailed", choices=["brief", "balanced", "detailed"])
    args = parser.parse_args()
    out = args.out
    out.mkdir(parents=True, exist_ok=True)

    # 1. "Capture": the repo's handwriting fixture is the visible page; the lasso
    #    is around "7 + 3 = ____" (virtual rect, as detect_selection_rect returns).
    page = Image.open(REPO / "evaluations/blank_math/input.png")
    sx, sy = page.width / W, page.height / H
    selection = {"x": 109, "y": 208, "w": 372, "h": 71}
    crop = page.crop((int(selection["x"] * sx), int(selection["y"] * sy),
                      int((selection["x"] + selection["w"]) * sx), int((selection["y"] + selection["h"]) * sy)))

    def b64(img):
        buf = tempfile.SpooledTemporaryFile()
        img.convert("RGB").save(buf, "PNG")
        buf.seek(0)
        return base64.b64encode(buf.read()).decode()

    # 2. Placement before submission (append_rect: below the lowest ink, never above).
    bottom = ink_bottom(page)
    rect = rect_after(max(bottom or 55, selection["y"] + selection["h"]), selection["x"])

    # 3. The request, as coordinator.rs + openai.rs assemble it.
    prompt_file = json.loads((REPO / "prompts/selection_concise_ink.json").read_text())
    tool = json.loads((REPO / "prompts/tool_draw_answer.json").read_text())
    max_chars = max(12, min(52, int((rect["w"] - 28) / 13.2)))
    prompt = prompt_file["prompt"]
    prompt += (f"\nReply layout: use at most 128 lines, each at most {max_chars} characters. "
               "Answer all parts of the question concisely.")
    prompt += ("\nReply pagination: additional note pages are available. Complete the answer; "
               "the application handles page breaks at the original font size.")
    content = [{"type": "text", "text": "Image 1: selected handwriting, the latest user question."},
               {"type": "image_url", "image_url": {"url": "data:image/png;base64," + b64(crop)}},
               {"type": "text", "text": "Image 2: surrounding visible page, including earlier user notes and AI replies. Off-screen content is not included."},
               {"type": "image_url", "image_url": {"url": "data:image/png;base64," + b64(page)}},
               {"type": "text", "text": prompt}]
    settings = {"backend": args.live or "codex", "reply_length": args.reply_length, "page_context": True,
                "auto_fallback": False, "backend_order": [args.live or "codex"], "models": {}}
    body = {"model": "codex", "messages": [{"role": "user", "content": content}],
            "tools": [{"type": "function", "function": {k: tool[k] for k in ("name", "description", "parameters")}}],
            "tool_choice": "required", "parallel_tool_calls": False, "remarkable_settings": settings}
    raw = json.dumps(body).encode()

    # 4. The real bridge, on an ephemeral loopback port, with a fresh token.
    work = Path(tempfile.mkdtemp(prefix="sr-eval-"))
    os.environ["FAKE_CODEX_RECORD"] = str(out)
    os.environ["FAKE_CODEX_ANSWER"] = str(REPO / "scripts/eval/stub_answer.json")
    (out / "stub-invocations.txt").unlink(missing_ok=True)
    if args.live:
        executable = shutil.which("codex") or "codex"
    elif os.name == "nt":
        executable = str(work / "codex.cmd")
        Path(executable).write_text(f'@"{sys.executable}" "{REPO / "scripts/eval/fake_codex.py"}" %*\r\n')
    else:
        executable = str(work / "codex")
        Path(executable).write_text(f'#!/bin/sh\nexec "{sys.executable}" "{REPO / "scripts/eval/fake_codex.py"}" "$@"\n')
        os.chmod(executable, 0o700)
    token = secrets.token_urlsafe(36)
    server = codex_bridge.Bridge(0, token, mode="ink", executable=executable, timeout=360)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{server.server_port}"
    try:
        health = json.load(urllib.request.urlopen(base + "/health", timeout=4))  # check_bridge
        key = "rm-eval-" + secrets.token_hex(8)

        def post(retry):
            req = urllib.request.Request(base + "/v1/chat/completions", data=raw, method="POST", headers={
                "Authorization": "Bearer " + token, "Content-Type": "application/json",
                "Idempotency-Key": key, "X-Remarkable-Retry": "1" if retry else "0"})
            with urllib.request.urlopen(req, timeout=420) as response:
                return response.status, json.load(response)

        status, response = post(False)
        # The device's lost-response path: same ID, retry=1 must return the
        # receipt without running the agent again.
        retry_status, retry_response = post(True)
        unauthorized = urllib.request.Request(base + "/v1/chat/completions", data=raw, method="POST",
                                              headers={"Authorization": "Bearer wrong", "Content-Type": "application/json"})
        try:
            urllib.request.urlopen(unauthorized, timeout=5)
            auth_check = "accepted (BUG)"
        except urllib.error.HTTPError as error:
            auth_check = f"rejected with HTTP {error.code}"
    finally:
        server.shutdown()
        server.server_close()
        shutil.rmtree(work, ignore_errors=True)

    # 5. Device side: parse the forced tool call like openai.rs, then lay it out.
    call = response["choices"][0]["message"]["tool_calls"][0]["function"]
    answer = json.loads(call["arguments"])
    DRAWINGS[:] = answer.get("illustrations", [])
    plan = plan_pages(answer["lines"], DRAWINGS, rect)
    for i, img in enumerate(render(plan, page), 1):
        img.save(out / f"page-{i}.png")

    invocations = int((out / "stub-invocations.txt").read_text()) if (out / "stub-invocations.txt").exists() else None
    summary = {
        "model": "LIVE " + args.live if args.live else "STUB (scripts/eval/stub_answer.json via scripts/eval/fake_codex.py)",
        "health": health, "selection_rect": selection, "ink_bottom": bottom, "answer_rect": rect,
        "reply_layout_max_chars": max_chars, "http_status": status, "remarkable_backend": response.get("remarkable_backend"),
        "tool": call["name"], "answer_lines_after_bridge": answer["lines"],
        "illustrations": [d["title"] for d in DRAWINGS],
        "retry_same_id": {"status": retry_status, "identical": retry_response == response,
                          "agent_invocations": invocations},
        "wrong_token": auth_check, "page_plan": plan,
    }
    (out / "turn.json").write_text(json.dumps(summary, indent=1) + "\n")
    redacted = json.loads(raw)
    for item in redacted["messages"][0]["content"]:
        if item["type"] == "image_url":
            item["image_url"]["url"] = item["image_url"]["url"][:40] + "...(PNG elided)"
    (out / "request.json").write_text(json.dumps(redacted, indent=1) + "\n")
    print(textwrap.dedent(f"""\
        model: {summary['model']}
        bridge health: {health}
        answer rect: {rect} (ink bottom {bottom}); reply width {max_chars} chars
        HTTP {status}; backend {response.get('remarkable_backend')}
        {len(answer['lines'])} lines after the bridge, {len(DRAWINGS)} illustration(s)
        retry with same id: HTTP {retry_status}, identical={retry_response == response}, agent runs={invocations}
        wrong token: {auth_check}
        pages: {[[('%d lines' % len(a['lines'])) if 'lines' in a else a['illustration'] for a in p['areas']] for p in plan['pages']]}
        outputs in {out}"""))


if __name__ == "__main__":
    main()

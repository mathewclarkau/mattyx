#!/usr/bin/env python3
"""Zero-dependency static documentation site generator for mattyx (issue #142).

Syncs `mux/docs/protocol.md` -> `docs-site/src/protocol.md` and
`mux/docs/configuration.md` -> `docs-site/src/configuration.md` (with a
`--check` mode that fails CI if the synced files drift), and renders all pages
into `docs-site/dist/` as zero-JS static HTML.
"""

from __future__ import annotations

import argparse
import html
import re
import shutil
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SITE_DIR = REPO_ROOT / "docs-site"
SRC_DIR = SITE_DIR / "src"
DIST_DIR = SITE_DIR / "dist"

SYNC_PAIRS = [
    (
        REPO_ROOT / "mux" / "docs" / "protocol.md",
        SRC_DIR / "protocol.md",
        "<!-- AUTO-GENERATED FROM mux/docs/protocol.md BY scripts/sync-docs-site.sh — DO NOT EDIT DIRECTLY -->\n\n",
    ),
    (
        REPO_ROOT / "mux" / "docs" / "configuration.md",
        SRC_DIR / "configuration.md",
        "<!-- AUTO-GENERATED FROM mux/docs/configuration.md BY scripts/sync-docs-site.sh — DO NOT EDIT DIRECTLY -->\n\n",
    ),
]

PAGES = [
    ("index", "Quickstart", "index.md"),
    ("agents", "Agent Integrations", "agents.md"),
    ("protocol", "CLI & Socket API", "protocol.md"),
    ("configuration", "Configuration", "configuration.md"),
    ("comparison", "Comparison", "comparison.md"),
    ("changelog", "Changelog", "changelog.md"),
]

CSS = """\
:root {
  --bg: #0f1117;
  --panel: #161922;
  --border: #262b38;
  --text: #e6edf3;
  --muted: #9ba3b4;
  --accent: #7aa2f7;
  --code-bg: #1a1e29;
}
* { box-sizing: border-box; }
body {
  margin: 0;
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
  background: var(--bg);
  color: var(--text);
  line-height: 1.6;
}
header {
  background: var(--panel);
  border-bottom: 1px solid var(--border);
  padding: 0.85rem 1.5rem;
  position: sticky;
  top: 0;
  z-index: 10;
}
.nav-wrap {
  max-width: 1080px;
  margin: 0 auto;
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
}
.brand {
  font-weight: 700;
  font-size: 1.15rem;
  color: var(--text);
  text-decoration: none;
  font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
}
.brand span { color: var(--accent); }
nav {
  display: flex;
  flex-wrap: wrap;
  gap: 0.9rem;
}
nav a {
  color: var(--muted);
  text-decoration: none;
  font-size: 0.92rem;
  padding: 0.25rem 0.5rem;
  border-radius: 4px;
}
nav a:hover, nav a.active {
  color: var(--text);
  background: var(--code-bg);
}
main {
  max-width: 1080px;
  margin: 2rem auto;
  padding: 0 1.5rem 4rem;
}
h1, h2, h3, h4 {
  color: #ffffff;
  margin-top: 1.8rem;
  margin-bottom: 0.6rem;
  line-height: 1.3;
}
h1 {
  font-size: 2rem;
  border-bottom: 1px solid var(--border);
  padding-bottom: 0.5rem;
}
h2 {
  font-size: 1.45rem;
  border-bottom: 1px solid var(--border);
  padding-bottom: 0.35rem;
}
a { color: var(--accent); }
pre {
  background: var(--code-bg);
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 0.9rem 1rem;
  overflow-x: auto;
  font-size: 0.88rem;
}
code {
  font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
  background: var(--code-bg);
  padding: 0.15rem 0.35rem;
  border-radius: 4px;
  font-size: 0.88em;
}
pre code {
  background: transparent;
  padding: 0;
}
table {
  width: 100%;
  border-collapse: collapse;
  margin: 1.2rem 0;
  font-size: 0.92rem;
}
th, td {
  border: 1px solid var(--border);
  padding: 0.55rem 0.75rem;
  text-align: left;
  vertical-align: top;
}
th {
  background: var(--panel);
  color: #ffffff;
}
tr:nth-child(even) td {
  background: rgba(255, 255, 255, 0.015);
}
hr {
  border: 0;
  border-top: 1px solid var(--border);
  margin: 2rem 0;
}
footer {
  max-width: 1080px;
  margin: 0 auto;
  padding: 1.5rem;
  border-top: 1px solid var(--border);
  color: var(--muted);
  font-size: 0.85rem;
}
"""


def slugify(text: str) -> str:
    clean = re.sub(r"<[^>]+>", "", text)
    clean = re.sub(r"[^\w\s-]", "", clean.lower()).strip()
    return re.sub(r"[\s]+", "-", clean)


def render_inline(text: str) -> str:
    # Escape HTML first, preserving code spans via placeholders
    code_spans: list[str] = []

    def save_code(match: re.Match[str]) -> str:
        idx = len(code_spans)
        code_spans.append(f"<code>{html.escape(match.group(1))}</code>")
        return f"\x00CODE{idx}\x00"

    text = re.sub(r"`([^`]+)`", save_code, text)
    text = html.escape(text)

    # Links: [label](url)
    text = re.sub(
        r"\[([^\]]+)\]\(([^)]+)\)",
        lambda m: f'<a href="{m.group(2)}">{m.group(1)}</a>',
        text,
    )
    # Bold + Italic
    text = re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", text)
    text = re.sub(r"\*([^*]+)\*", r"<em>\1</em>", text)
    text = re.sub(r"~~([^~]+)~~", r"<del>\1</del>", text)

    for idx, span in enumerate(code_spans):
        text = text.replace(f"\x00CODE{idx}\x00", span)
    return text


def split_table_row(line: str) -> list[str]:
    stripped = line.strip()
    if stripped.startswith("|"):
        stripped = stripped[1:]
    if stripped.endswith("|"):
        stripped = stripped[:-1]
    return [cell.strip() for cell in stripped.split("|")]


def markdown_to_html(md: str) -> str:
    lines = md.splitlines()
    out: list[str] = []
    i = 0
    n = len(lines)

    while i < n:
        line = lines[i]

        # Skip HTML comments
        if line.strip().startswith("<!--") and line.strip().endswith("-->"):
            i += 1
            continue

        # Fenced code block
        if line.startswith("```"):
            lang = line[3:].strip()
            code_lines: list[str] = []
            i += 1
            while i < n and not lines[i].startswith("```"):
                code_lines.append(lines[i])
                i += 1
            i += 1  # skip closing ```
            cls = f' class="language-{html.escape(lang)}"' if lang else ""
            out.append(f"<pre><code{cls}>{html.escape(chr(10).join(code_lines))}</code></pre>")
            continue

        # Horizontal rule
        if re.match(r"^\s*---\s*$", line):
            out.append("<hr />")
            i += 1
            continue

        # Heading
        m_head = re.match(r"^(#{1,6})\s+(.*)$", line)
        if m_head:
            level = len(m_head.group(1))
            content = render_inline(m_head.group(2).strip())
            slug = slugify(m_head.group(2))
            out.append(f'<h{level} id="{slug}">{content}</h{level}>')
            i += 1
            continue

        # Markdown table
        if (
            "|" in line
            and i + 1 < n
            and re.match(r"^\s*\|?(\s*:?-+:?\s*\|)+\s*:?-+:?\s*\|?\s*$", lines[i + 1])
        ):
            headers = split_table_row(line)
            i += 2  # skip header + separator
            rows: list[list[str]] = []
            while i < n and "|" in lines[i] and lines[i].strip() != "":
                rows.append(split_table_row(lines[i]))
                i += 1
            thead = "".join(f"<th>{render_inline(h)}</th>" for h in headers)
            tbody_rows = []
            for r in rows:
                cells = "".join(f"<td>{render_inline(c)}</td>" for c in r)
                tbody_rows.append(f"<tr>{cells}</tr>")
            out.append(f"<table><thead><tr>{thead}</tr></thead><tbody>{''.join(tbody_rows)}</tbody></table>")
            continue

        # Unordered list
        if re.match(r"^\s*[-*]\s+", line):
            items: list[str] = []
            while i < n and re.match(r"^\s*[-*]\s+", lines[i]):
                item_text = re.sub(r"^\s*[-*]\s+", "", lines[i])
                i += 1
                while i < n and lines[i].startswith("  ") and not re.match(r"^\s*[-*0-9]", lines[i]):
                    item_text += " " + lines[i].strip()
                    i += 1
                items.append(f"<li>{render_inline(item_text)}</li>")
            out.append(f"<ul>{''.join(items)}</ul>")
            continue

        # Ordered list (keeps one <ol> across blank lines and nested bullets)
        m_ol = re.match(r"^\s*(\d+)\.\s+", line)
        if m_ol:
            start_num = int(m_ol.group(1))
            items = []
            while i < n and re.match(r"^\s*\d+\.\s+", lines[i]):
                item_text = re.sub(r"^\s*\d+\.\s+", "", lines[i])
                i += 1
                while i < n and lines[i].startswith("   ") and not re.match(r"^\s*[-*0-9]", lines[i]):
                    item_text += " " + lines[i].strip()
                    i += 1
                item_html = render_inline(item_text)
                # Absorb nested bullet blocks into this list item
                while i < n and re.match(r"^\s+[-*]\s+", lines[i]):
                    sub_items: list[str] = []
                    while i < n and re.match(r"^\s+[-*]\s+", lines[i]):
                        sub_text = re.sub(r"^\s*[-*]\s+", "", lines[i])
                        i += 1
                        while i < n and lines[i].startswith("  ") and not re.match(r"^\s*[-*0-9]", lines[i]):
                            sub_text += " " + lines[i].strip()
                            i += 1
                        sub_items.append(f"<li>{render_inline(sub_text)}</li>")
                    item_html += f"<ul>{''.join(sub_items)}</ul>"
                items.append(f"<li>{item_html}</li>")
                # Skip blank lines; if another ordered item follows, continue the same list
                j = i
                while j < n and lines[j].strip() == "":
                    j += 1
                if j < n and re.match(r"^\s*\d+\.\s+", lines[j]):
                    i = j
            start_attr = f' start="{start_num}"' if start_num != 1 else ""
            out.append(f"<ol{start_attr}>{''.join(items)}</ol>")
            continue

        # Blank line
        if line.strip() == "":
            i += 1
            continue

        # Paragraph
        para: list[str] = [line.strip()]
        i += 1
        while (
            i < n
            and lines[i].strip() != ""
            and not lines[i].startswith("```")
            and not lines[i].startswith("#")
            and not re.match(r"^\s*---\s*$", lines[i])
            and not re.match(r"^\s*[-*]\s+", lines[i])
            and not re.match(r"^\s*\d+\.\s+", lines[i])
        ):
            para.append(lines[i].strip())
            i += 1
        out.append(f"<p>{render_inline(' '.join(para))}</p>")

    return "\n".join(out)


def sync_sources(check_only: bool) -> int:
    drifted = False
    for src_path, dst_path, banner in SYNC_PAIRS:
        expected = banner + src_path.read_text(encoding="utf-8")
        if check_only:
            if not dst_path.exists():
                print(
                    f"docs-site drift: {dst_path.relative_to(REPO_ROOT)} is missing (run scripts/sync-docs-site.sh)",
                    file=sys.stderr,
                )
                drifted = True
                continue
            actual = dst_path.read_text(encoding="utf-8")
            if actual != expected:
                print(
                    f"docs-site drift: {dst_path.relative_to(REPO_ROOT)} is out of sync with {src_path.relative_to(REPO_ROOT)} "
                    f"(run scripts/sync-docs-site.sh)",
                    file=sys.stderr,
                )
                drifted = True
        else:
            dst_path.parent.mkdir(parents=True, exist_ok=True)
            dst_path.write_text(expected, encoding="utf-8")
    return 1 if drifted else 0


def build_site() -> None:
    if DIST_DIR.exists():
        shutil.rmtree(DIST_DIR, ignore_errors=True)
    DIST_DIR.mkdir(parents=True, exist_ok=True)
    (DIST_DIR / "style.css").write_text(CSS, encoding="utf-8")
    (DIST_DIR / ".nojekyll").write_text("", encoding="utf-8")

    for slug, title, filename in PAGES:
        md_path = SRC_DIR / filename
        md_content = md_path.read_text(encoding="utf-8")
        body_html = markdown_to_html(md_content)

        nav_links = []
        for nav_slug, nav_title, _ in PAGES:
            cls = ' class="active"' if nav_slug == slug else ""
            nav_links.append(f'<a href="./{nav_slug}.html"{cls}>{html.escape(nav_title)}</a>')
        nav_links.append('<a href="https://github.com/mathewclarkau/mattyx">GitHub</a>')

        page_html = f"""<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>{html.escape(title)} — mtyx (mattyx) Documentation</title>
  <link rel="stylesheet" href="./style.css" />
</head>
<body>
  <header>
    <div class="nav-wrap">
      <a class="brand" href="./index.html"><span>mtyx</span> // mattyx</a>
      <nav>
        {" ".join(nav_links)}
      </nav>
    </div>
  </header>
  <main>
{body_html}
  </main>
  <footer>
    mtyx (mattyx) — GPL-3.0-or-later · Built from in-repo docs (<a href="https://github.com/mathewclarkau/mattyx">mathewclarkau/mattyx</a>)
  </footer>
</body>
</html>
"""
        (DIST_DIR / f"{slug}.html").write_text(page_html, encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(description="Sync and build mattyx static docs site.")
    parser.add_argument(
        "--check",
        action="store_true",
        help="Verify that synced docs match mux/docs/ without modifying tracked files, and verify site builds.",
    )
    args = parser.parse_args()

    rc = sync_sources(check_only=args.check)
    if rc != 0:
        return rc
    build_site()
    print(f"docs-site: built {len(PAGES)} static pages in {DIST_DIR.relative_to(REPO_ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

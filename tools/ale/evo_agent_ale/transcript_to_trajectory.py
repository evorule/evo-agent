"""ATIF-v1.8 → ALE-v1.0 trajectory mapping.

Reads the ATIF trajectory exported by ``GET /api/sessions/{id}/atif``
(evo-agent's journal × audit-chain × transcript join) and populates an
ALE ``TrajectoryBuilder`` with ALE-v1.0 steps:

* ATIF folds each turn into one ``agent`` step carrying the assistant
  message, the tool calls, the per-call observation results and the LLM
  metrics. ALE-v1.0 wants the environment's response as its own
  ``environment`` step, so each agent step is split in two.
* ATIF observation results store the tool output as plain text. When the
  tool result carried side-channel images (evo-agent's multimodal上行
  pipeline), the text embeds an ``images_extracted`` fingerprint list and
  the image bytes live content-addressed under ``<serve workdir>/data/
  agent-images/``. Those files are re-read here and re-attached as inline
  base64 image ContentParts — the ALE framework moves them to
  ``screenshots/NNNN.png`` at persistence time.
* Per-step ``prompt_tokens``/``completion_tokens`` map to
  ``StepMetrics.input_tokens``/``output_tokens``; authoritative totals are
  recorded via ``builder.override_final_metrics`` (finalize itself stays
  with the framework).

Everything here is a pure function of (builder, atif, images_dir) — no I/O
beyond reading the referenced image files.
"""

from __future__ import annotations

import base64
import json
import logging
from pathlib import Path
from typing import Any

logger = logging.getLogger(__name__)

# media_type → file extension, mirroring evo-agent's agent-images store naming.
_MEDIA_EXT: dict[str, str] = {
    "image/png": "png",
    "image/jpeg": "jpg",
    "image/gif": "gif",
    "image/webp": "webp",
}

_IMAGES_EXTRACTED_KEY = "images_extracted"


def populate_builder_from_atif(
    builder: Any,
    atif: dict[str, Any],
    images_dir: Path,
) -> None:
    """Map an ATIF trajectory object onto ``builder`` (ALE-v1.0 steps)."""
    for step in atif.get("steps") or []:
        source = step.get("source")
        if source == "agent":
            _add_agent_step(builder, step, images_dir)
        elif source == "user":
            builder.add_step(source="user", message=step.get("message"))
        elif source == "system":
            builder.add_step(
                source="system",
                message=step.get("message"),
                extra=_clean_extra(step.get("extra")),
            )
        else:
            logger.warning("atif: unknown step source %r — skipped", source)

    final = atif.get("final_metrics") or {}
    builder.override_final_metrics(
        total_input_tokens=final.get("total_prompt_tokens"),
        total_output_tokens=final.get("total_completion_tokens"),
    )


def _add_agent_step(builder: Any, step: dict[str, Any], images_dir: Path) -> None:
    tool_calls = [
        {
            "id": call.get("tool_call_id") or "",
            "name": call.get("function_name") or "",
            "arguments": call.get("arguments") or {},
        }
        for call in (step.get("tool_calls") or [])
    ]

    metrics = None
    raw_metrics = step.get("metrics")
    if raw_metrics:
        metrics = {
            "input_tokens": raw_metrics.get("prompt_tokens"),
            "output_tokens": raw_metrics.get("completion_tokens"),
        }

    builder.add_step(
        source="agent",
        message=step.get("message") or None,
        tool_calls=tool_calls or None,
        metrics=metrics,
        extra=_clean_extra(step.get("extra")),
    )

    observation = step.get("observation")
    if not observation:
        return

    results = []
    env_messages: list[str] = []
    for result in observation.get("results") or []:
        call_id = result.get("source_call_id")
        content_parts = _content_parts(result.get("content"), images_dir)
        if call_id:
            results.append({"tool_call_id": call_id, "content": content_parts})
        else:
            # No tool call to align with (ATIF sidecar/system observations):
            # ALE carries these as a plain environment message.
            for part in content_parts:
                if part.get("type") == "text" and part.get("text"):
                    env_messages.append(part["text"])
    if results:
        builder.add_step(source="environment", observation={"results": results})
    if env_messages:
        builder.add_step(
            source="environment",
            message="\n".join(env_messages),
            observation={"results": []},
        )


def _content_parts(content: Any, images_dir: Path) -> list[dict[str, Any]]:
    """Tool output text → ALE ContentParts, re-attaching stored screenshots."""
    parts: list[dict[str, Any]] = []
    text = content if isinstance(content, str) else None
    if text is not None:
        parts.append({"type": "text", "text": text})
    for image in _image_refs(text, images_dir):
        media_type, data = image
        parts.append(
            {
                "type": "image",
                "image": {
                    "type": "base64",
                    "data": data,
                    "media_type": media_type,
                },
            }
        )
    return parts


def _image_refs(text: str | None, images_dir: Path) -> list[tuple[str, str]]:
    """(media_type, base64) pairs referenced by an ``images_extracted`` block."""
    if not text or _IMAGES_EXTRACTED_KEY not in text:
        return []
    try:
        obj = json.loads(text)
    except (ValueError, TypeError):
        return []
    if not isinstance(obj, dict):
        return []
    out: list[tuple[str, str]] = []
    for item in obj.get(_IMAGES_EXTRACTED_KEY) or []:
        if not isinstance(item, dict):
            continue
        fingerprint = item.get("fingerprint")
        media_type = item.get("mime_type")
        ext = _MEDIA_EXT.get(media_type or "")
        if not fingerprint or not ext:
            continue
        path = images_dir / f"{fingerprint}.{ext}"
        try:
            data = base64.b64encode(path.read_bytes()).decode("ascii")
        except OSError as exc:
            logger.warning("atif: image %s unreadable: %s", path, exc)
            continue
        out.append((media_type, data))
    return out


def _clean_extra(extra: Any) -> dict[str, Any] | None:
    return dict(extra) if isinstance(extra, dict) and extra else None

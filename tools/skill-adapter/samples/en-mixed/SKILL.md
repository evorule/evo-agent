---
name: report-audit-flow
version: 1.2.0
description: English mixed-form regression sample (rewritten from real-skill structural patterns, business semantics sanitized)
---

## When to Use This Skill

Audit report generation, approval chain checks, and log verification tasks.

## Requirements for Every Output

- Must include the approval chain reference in every report
- Never skip the retention record when an amount field is modified
- Always run the consistency check before submission
- Route submissions → submit_gate before delivery

## Step 1: Collect the Input

Gather the source records and verify the status field matches the approval chain.

## Step 2: Run the Verification

```json
{"instruction_type": "audit_check", "scope": "daily", "strict": true}
```

## Common Pitfalls

- Skipping the dual-review when amounts exceed the threshold
- Writing reports without the traceable evidence block

## Quick Reference

| Check | Tool |
|---|---|
| Chain consistency | audit_tool |
| Log completeness | log_checker |

## Background

This sample is a sanitized regression fixture; it carries no real business semantics.

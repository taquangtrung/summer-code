---
name: Feature request
about: Propose something the agent should be able to do
labels: enhancement
---

## The problem

What you were trying to do, and where the current behaviour stopped you.

## What you have in mind

## How it fits the existing contracts

`docs/ARCHITECTURE.md` lists the design contracts, and a proposal that breaks one needs to say which and why the trade is worth it. The load-bearing ones are usually: every access is checked before it happens, the journal is the only source of truth, and nothing claims enforcement the OS is not actually backing.

## Alternatives you considered

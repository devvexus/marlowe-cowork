# Kimi K2.6 released

Moonshot AI today released Kimi K2.6, the newest model in the Kimi K2 line, on 2026-10-20.

Kimi K2.6 is a mixture-of-experts model with 1 trillion total parameters and 32 billion active parameters per token. It supports a context window of 256k tokens and is available as open weights.

Compared with [Kimi K2.5](https://moonshot.example/blog/kimi-k2-5), the new release improves long-context retrieval, tool use and agentic coding. The full details, including the training recipe and every benchmark table, are in the [K2.6 technical report](https://moonshot.example/papers/k2-6).

## Availability

The model is available today through the Kimi API and as downloadable weights under a permissive licence. Pricing is unchanged from K2.5, and the existing API model names keep working. Moonshot says the next model in the line is still in training and has no release date.

## What changed

Context handling was rebuilt so that the full 256k window is usable in practice rather than only in principle. Moonshot reports that long-document question answering improved most, and that the model follows multi-step instructions more reliably than K2.5 did.

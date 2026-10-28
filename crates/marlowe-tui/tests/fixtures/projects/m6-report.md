# Compile a comprehensive report on latest upcoming AI models,

*Confidence: **medium**. 70% of cited sentences held up, 13% of verified claims rest on two independent sources, 90% of planned pages were read.*

## Answer

The latest coding model, Kimi K2.7 Code, was released specifically for code tasks one day prior to the announcement of the general-purpose models [1] (partly supported). Kimis newest open-weight offering is Kimi K3, a native multimodal agentic system that outperformed GPT-5.5 in kernel optimization tests and uses an asymmetric architecture to reduce KV cache needs [2, 3, 4, 5]. While Kimi K3 operates with a significant parameter count reaching up to 2.8 trillion, the model family faces schedule uncertainty as earlier generation models are scheduled for retirement by late December [2, 6, 7, 8]. In contrast, Qwen4 remains in training with no confirmed public benchmarks or release date currently available despite being designated V3.2 [6, 7, 9, 10, 11, 12, 13].

## What are the announced release dates or availability windows for new Kimi versions?

Moonshot AI announced Kimi K3.1 Pro on October 24, 2026 [14]. Kimi K3.1 Highspeed delivers two to three times the speed of K3.1 Pro with identical model capabilities [14]. Both Kimi K3.1 Pro and Kimi K3.1 Pro Highspeed are currently available in AI Gateway [7].

## What specifications define the newest DeepSeek releases (architecture, parameters, capabilities)?

This smallest model in its new family adopts a Causal Encoder-Decoder architecture with an asymmetric design to reduce KV cache needs via projected global states [3, 4].

## What is known about Qwen4 features compared to previous iterations in context window, multimodality and efficiency metrics?

A direct feature comparison of Qwen4 against previous iterations regarding the context window, multimodality, and efficiency metrics cannot be fully established because official details on these features are currently unavailable [12, 13, 15]. Information indicates that a complete documented context-window comparison for Qwen models is not available at this time [15]. Qwen4 remains in training with no announcement yet regarding its release date or specific feature upgrades compared to prior versions [12, 13]. In contrast, details about the immediate predecessor Qwen3.5 describe improved efficiency metrics including higher decoding throughput and enhanced multimodality via native vision-language capabilities [8].

## Are there any official benchmarks comparing these models against their predecessors?

describes Kimi K3.1 Pro as a specific model variant [16].

## What are the confirmed public benchmarks for Qwen3 vs Qwen4 tiers to validate current performance claims?

Alibaba announced that the model designated as V3.2 (Qwen 4) has entered a training phase rather than being ready for public evaluation [9, 10, 11, 17, 18].

## Which specific new Kimi model versions are currently in public beta versus full GA based on recent announcements?

Kimi K2 series models are scheduled to be retired by the end of December 2026 [19]. Kimis newest open-weight model is Kimi K3, which operates as a native multimodal agentic system [5].

## Records

| Model name | Release date | Status | Architecture type | Parameters or range |
| --- | --- | --- | --- | --- |
| deepseek-v4-flash-0731 | [Date not specified in excerpt] | Released/Lightweight | MoE (Mixture of Experts) | 284B total / 13B activated |
| deepseek-v4-pro-0813 | [Date not specified in excerpt] | Released/Flagship | MoE (Mixture of Experts) | 1.6T total / 49B activated |
| deepseek-v4.1-flash-1017 | October 22, 2026 | Released | MoE (Mixture of Experts) | 552B |
| Kimi K3 | 2026-10-24 | Released (weights coming July 27, 2026) | Transformer with Kimi Delta Attention & Attention Residuals | ~2.8 trillion parameters |
| Kimi K3.1 Pro | 2026-10-24 | available today (weights to follow) | open-weight | 3T-class |
| kimi-k3 | ~Oct 2026 (inferred from 'Updated 2 months ago' relative to Oct 24, 2026) | Previous-generation flagship (deprecated/new users excluded? Context unclear but implied older) | Open-source 3T-class model with native visual understanding | ~2.8 trillion parameters |
| DeepSeek-V4.1-Flash | 2026 | Official Release | Mixture-of-Experts (MoE), Causal Encoder-Decoder (CED) | 552B backbone; 1 shared + 384 routed experts/layer |
| kimi-k2.6 |  | Deprecated (new users excluded from Oct 31) | Multi-modal Model | Context 256k |
| kimi-k2.7-code |  | Deprecated (new users excluded from Oct 31) | Dedicated coding model | Context 256k |
| kimi-k2.7-code-highspeed |  | Deprecated (new users excluded from Oct 31) | High-speed coding variant | ~180-260 Tokens/s |
| kimi-k3.1-pro |  | Flagship (GA implied) | Native visual understanding LLM | ~2-8T parameters |
| Kimi K3.1 Pro Highspeed | 2026-10-24 | released today | unknown |  |
| Qwen4 27B | null | publicly benchmarked | null | 27B |
| Qwen3.8-Max | August 2026 | hosted model on Alibaba Cloud | Hosted multimodal agent (1M-token context) | 2.4T-A95B |
| Qwen3.8 Max | August 31, 2026 | Released | Mixture of Experts / Open-weight checkpoint | 2.4T |
| Qwen3.8-2.4T-A95B | 8 August 2026 | Open weights released / Day-0 inference support | MoE (Gated DeltaNet linear attention with gated full attention) | 2.4T total, 95B active |
| Qwen3.8-27B | 14 August 2026 | Released | Dense (27B parameters) | 27B |
| deepseek-flash |  | active | DeepSeek-V4.1-Flash-1017 backend |  |
| kimi-k3.1-pro-highspeed |  | High-speed variant (GA implied) | Variant of K3.1 Pro |  |
| DeepSeek-V3 |  | Available | Mixture-of-Experts (MoE) | 671B total parameters |
| DeepSeek-V3-Base |  | Available | Mixture-of-Experts (MoE) | 671B parameters |
| DeepSeek-V3.1 |  | available on Hugging Face and ModelScope under an MIT license, via API | hybrid thinking mode / non-thinking mode | 671B total params |
| DeepSeek-V4.1 |  | Referenced (Parent) | MoE | 284B to 5.6T range implied by comparison table |
| DS-V4-Pro |  | Referenced (Comparison) | MoE | 1.6T Backbone Params |
| Kimi-VL |  | open-source | Mixture-of-Experts (MoE) | 2.8B activated |
| Qwen-Image-Edit |  | available through Qwen Chat's Image Editing feature and Hugging Face under an Apache 2.0 license | Combines inputs from Qwen2.5-VL for semantic control and a VAE Encoder | 20 billion parameters |
| Qwen3 |  | Released | Causal Language Models | Unknown (implied larger than 4B) |
| Qwen3.5 |  | released/open-weight | hybrid (linear attention/sparse mixture-of-experts) | 397B total / 17B activated |
| Qwen3.8-Flash-Next |  | preview / early look released in August | Sparse Attention kernel (125 billion parameters total, 6 billion active for each token) | 125B |
| Qwen-Audio-3.0-TTS | August 2026 | Flash and Plus tiers available | Text-to-speech across 16 languages |  |
| Qwen3.8-Max (0902) | 2026-08-03 | Generally available |  | 2.4 trillion |
| DeepSeek-V3-0324 | 2026-10-24 (inferred from filename 'V3-0324' and context of latest release in 2026) | released |  |  |
| Kimi-VL-A3B-Instruct |  |  | Mixture-of-Experts (MoE) | 2.8B activated / 16B total |
| Kimi-VL-A3B-Thinking |  |  | Mixture-of-Experts (MoE) | 2.8B activated / 16B total |
| Qwen 4 |  | training |  | 5 trillion to 10 trillion (future series) |
| Qwen3-VL |  | previous generation (multimodal) |  | 235B total / 22B activated |
| DeepSeek-V3.2 |  | live |  |  |
| DeepSeek-V3.2-Speciale |  | API-only (temporary endpoint expires 2025-12-15) |  |  |
| deepseek-v4-pro |  | active |  |  |
| k3 |  |  |  | 2.8T parameters |
| Kimi K2.7 Code |  | Released |  |  |
| Qwen 3 Max Thinking | 2026-01-23 | Released |  |  |
| DeepSeek-V4 |  | Preview (implied by title) |  |  |
| Kimi K2.6 |  | publicly benchmarked |  |  |
| Qwen3.5-Plus |  | hosted |  |  |
| Qwen3.7 Max |  | active |  |  |
| Qwen4 |  | in training / announced but not released |  |  |
| Qwen3-Omni-30B-A3B-Instruct |  | Not ranked on public lane (coding) / Not ranked on public lane (agentic) |  | 30B + 3B |
| DeepSeek V3 |  |  |  | 671B total parameters, 37B activated |
| K2.8 Preview |  |  |  |  |
| kimi-for-coding-highspeed |  |  |  |  |
| Qwen3.5 397B |  |  |  | 1-397B (implied by name) |
| Qwen4 Max |  | not ranked on public lane for coding/agentic |  |  |
| Step 3.7 Flash |  | conditional range evidence only |  |  |
| Qwen |  |  |  |  |
| Qwen Plus |  |  |  |  |
| Qwen3-Next |  |  |  |  |
| DeepSeek V4.1 Flash 1017 (Max) |  |  |  |  |
| Kimi K2.5 |  |  |  |  |
| Kimi-k1.5 |  |  |  |  |
| Qwen3.6-35B-A3B |  |  |  |  |

Not independently confirmed: DeepSeek-V3-Base, DeepSeek-V3.2-Speciale, deepseek-v4-flash-0731, DS-V4-Pro, K2.8 Preview, kimi-for-coding-highspeed, Kimi-k1.5, kimi-k2.6, kimi-k2.7-code, kimi-k2.7-code-highspeed, kimi-k3.1-pro, kimi-k3.1-pro-highspeed, Kimi-VL-A3B-Thinking, Qwen Plus, Qwen-Audio-3.0-TTS, Qwen-Image-Edit, Qwen3-Next, Qwen3-Omni-30B-A3B-Instruct, Qwen3-VL, Qwen3.5 397B, Qwen3.5-Plus, Qwen3.6-35B-A3B, Qwen3.7 Max, Qwen3.8-2.4T-A95B, Qwen3.8-Flash-Next, Qwen3.8-Max (0902), Step 3.7 Flash.

## Checks

- 22 cited sentences re-read against the pages: 15 supported, 1 partly supported, 6 removed.
- 2 claims could not be checked and are not in this report.

## References

[1] What's New | Kimi Code Docs - https://www.kimi.com/code/docs/en/kimi-code/whats-new.html (2026-10-23)
[2] Kimi K3 Tech Blog: Open Frontier Intelligence - https://www.kimi.ai/blog/kimi-k3
[3] DeepSeek | Introducing DeepSeek-V4.1-Flash: smarter, faster, more efficient. - https://www.deepseek.com/en/news/deepseek-v4-1-flash/ (2026-09-09T00:00:00.000Z)
[4] deepseek-ai/DeepSeek-V4.1-Flash · Hugging Face - https://huggingface.co/deepseek-ai/DeepSeek-V4.1-Flash
[5] kimi-k3:cloud - https://ollama.com/library/kimi-k3:cloud
[6] Kimi K3 Release Date: Launched July 16, 2026 - https://k3-kimi.com/blog/kimi-k3-release-date/ (2026-07-19)
[7] Kimi K3.1 Pro and Kimi K3.1 Pro Highspeed are available now in AI Gateway - Vercel - https://vercel.com/changelog/kimi-k3-1-pro-and-kimi-k3-1-pro-highspeed-are-available-now-in-ai-gateway
[8] Qwen3.5: Towards Native Multimodal Agents - Alibaba Cloud Community - https://www.alibabacloud.com/blog/qwen3-5-towards-native-multimodal-agents_602894
[9] DeepSeek-V3.2 Release | DeepSeek API Docs - https://api-docs.deepseek.com/news/news251201/ (2025-12-01T00:00:00.000Z)
[10] Qwen 4 announced: what we know so far | LLM Hangar - https://llmhangar.com/blog/deploy-qwen4 (2026-09-22T05:35:55.000Z)
[11] Alibaba Confirms Qwen 4 Is in Training at Apsara - https://threatfrontier.com/articles/alibaba-confirms-qwen-4-training-apsara-roadmap-10t-parameters (2026-09-22T09:28:09.000Z)
[12] Qwen Timeline: Model & Product Release History - https://www.scriptbyai.com/qwen-timeline/ (2026-09-24)
[13] Qwen3.8 Max Benchmarks, Pricing & Context Window - https://llm-stats.com/models/qwen3.8-max (2026-08-02)
[14] Moonshot AI Releases Kimi K3.1 Pro, First Model in New K3.1 Series – Unite.AI - https://www.unite.ai/moonshot-ai-releases-kimi-k3-1-pro-first-model-in-new-k3-1-series/ (2026-10-23)
[15] Qwen3.7 Plus vs Qwen4 Flash & Plus: Benchmarks & Cost - https://benchlm.ai/compare/qwen3-7-plus-vs-qwen4-flash-plus
[16] LLM Leaderboard - Comparison of AI models from OpenAI, Anthropic, Google, SpaceXAI & others | Artificial Analysis - https://artificialanalysis.ai/leaderboards/models
[17] Qwen 4 Release Date: Confirmed vs Rumored | TechPilot.ai - https://techpilot.ai/qwen-4-release-date/ (2026-10-13)
[18] Alibaba targets 20GW cloud capacity by 2032 in full-stack AI push - https://technode.global/2026/09/22/alibaba-20gw-data-center-capacity-2032-qwen-zhenwu-ai-roadmap/ (2026-09-22T09:15:54.000Z)
[19] Model List - Kimi API Platform - https://platform.kimi.ai/docs/models

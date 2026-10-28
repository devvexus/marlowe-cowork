# Research latest upcoming AI models from Kimi, DeepSeek, and

*Confidence: **low**. 67% of cited sentences held up, 23% of verified claims rest on two independent sources, 86% of planned pages were read, 2 parts could not be written.*

## Answer

Kimi's latest release, K3.1 Pro, is positioned as its strongest model to date with a confirmed context window of one million tokens and an approximate parameter count in the trillion range [1, 2]. DeepSeek models are currently available via Workers AI without requiring setup or authentication for an instant browser preview of the V4.1 Flash variant [1, 2, 3, 4, 5, 6, 7] (partly supported). While Qwen has released Qwen3.8-Flash-Next as an architectural early preview featuring gated residual components and N-gram embedding tables, no official product named Qwen 4 or specific specifications for that model exist [8, 9].

## What are the most recent announced or expected releases of new Kimi models?

The most recently announced Kimi model, K3.1, supports a context window of one million tokens [1, 2]. This is the first model in the K3 series and represents their strongest 3 trillion parameter offering to date [1, 2].

## What is the current status of DeepSeek's newest model versions (V3.5)?

DeepSeek's newest frontier model is DeepSeek-V4.1-Flash, released on September 10, 2026 [10]. This version features a new Causal Encoder–Decoder architecture with a 552-billion-parameter Mixture-of-Experts configuration that activates only specific parameters based on input size [10, 11]. DeepSeek V4 Pro was also rolled out across APP, Web, and API with enhanced agent capabilities around August or September 2026 [12, 13] (partly supported).

## What information is available on upcoming Qwen4 model specifications?

*This section could not be written: form rule `cites_claims` rejected the reply on all 3 attempts.*

## What are the specific architectural differences and parameter counts officially disclosed for Qwen4 versus its predecessor?

The Qwen4-E series, specifically the latest iteration with updated specifications as of August 31, 2026, features a sparse mixture-of-experts architecture totaling 125 billion parameters [8, 9]. This model activates only 6 billion parameters per token while holding an additional 51 billion parameters in n-gram embedding tables off the accelerator. Official disclosures for Qwen4 versus its predecessor do not reveal specific architectural differences or parameter counts, with available launch information omitting such technical comparisons.

## When exactly was the DeepSeek-V4 family fully integrated into all official platforms following the October 17th rollout?

DeepSeek integrated its V4 family into official platforms starting on October 17, which marks the rollout of the DeepSeek-V4.1-Flash-1017 checkpoint [14, 15]. The model served by this integration is identified as deepseek-flash and utilizes a calling method that remains unchanged from previous versions [14, 15]. Cloudflare subsequently listed the DeepSeek-V4.1-Flash-1017 model on its Workers AI platform on October 19, indicating further integration into external infrastructure shortly after the initial rollout.

## Are there any pending Qwen3 or legacy updates that were mislabeled as Qwen4 in recent announcements versus the current release status.

Qwen3.8-Flash-Next acts as an early architecture preview for Qwen4 rather than a final release update. According to ChainCatcher citing Max For AI, Alibaba's Qwen models are in their final stage before the flagship release but no product named Qwen 4.0 officially exists as of October 2026.

## Are the parameters cited as 3 trillion for Kimi's K3.1 model officially verified by the company or community consensus rather than internal leaks?

Kimi K3 is the company's latest introduction, described as their most capable model. Sources state that Kimi K3 contains approximately 2.8 trillion parameters rather than exactly 3 trillion [16] (partly supported). One independent source refers to the K3.1 Pro variant as a 'most capable 3T-class model' but marks this specific detail with low reliability due to its singular origin and contested nature (0.70 score) [17] (partly supported). Another independent source from the same day notes that Moonshot said the model has about 3 trillion parameters, which is also marked with a low reliability score of 0.50 [18].

## Is DeepSeek-V4.1-Flash pricing accurately reflected in API documentation versus community reports given contested scores on token cost data?

DeepSeek released V4.5 on July 5, 2026 with hosted pricing of $0.50 per million input tokens and $1.10 per million output [19].

## Can the 'very soon' release timeline for Qwen4 be quantified into specific months based on recent development velocity or roadmaps since September 2026?

*This section could not be written: form rule `text_present` rejected the reply on all 3 attempts.*

## What architectural components distinguish Qwen3.8-Flash-Next from the anticipated full Qwen4 release given claims of it being an early preview?

Qwen3.8-Flash-Next serves as an early preview or experimental foundation for the architecture used in Qwen4. The model features a gated residual component that modulates information flow using specific read and write gates, utilizing four dynamic branches to control cross-layer interactions [5, 20, 21]. Token mixing operates at the micro-block level through a layer-wise hybrid of Gated DeltaNet combined with global attention rather than selecting individual tokens. The architecture utilizes 'N-gram Embedding' to index parameters and includes a separate 51 billion parameter n-gram table for local-pattern memory without routing all data through the compute path [5, 7]. This model upgrades existing components in attention, residual connections, embeddings, and optimization areas distinct from standard Qwen versions. The built-in Multi-Token Prediction module supports speculative decoding capabilities within the framework.

## Is Cloudflare Workers AI integration for DeepSeek V4 available to public API users or restricted to internal enterprise clients as implied by limited data points?

DeepSeek V4 Flash and Pro models are available on Workers AI, with DeepSeek V4.1 Flash also now live there. Users can try these models via the Workers AI LLM Playground without requiring setup or authentication for instant preview in the browser [22, 23]. Although access to both models requires a Workers Paid plan or prepaid AI Gateway credits, this requirement is marked as contested information.

## Records

| Model name | Release status | Specifications |
| --- | --- | --- |
| DeepSeek V4.1 Flash | Released September 10, 2026; refreshed in place as DeepSeek-V4.1-Flash-1017 on October 17, 2026. | 552-billion-parameter Mixture-of-Experts model in a new Causal Encoder–Decoder architecture that activates only 8B param |
| DeepSeek V4 Flash (0731) | Released on Workers AI | 1,048,576 token context; supports thinking mode and function calling |
| DeepSeek-V4-Pro | General Availability | Enhanced agent capabilities, supports Responses API |
| DeepSeek V4 Pro (0813) | Released on Workers AI | 1,048,576 token context; supersedes preview version with enhanced agentic capabilities |
| Kimi K2 | Available on web/mobile/app | Supports 256K context window; enhanced agentic coding capabilities |
| Kimi K3 | Released (Open Frontier Intelligence) | 2.8T parameters total, 104B activated; Kimi Delta Attention + Gated MLA attention; Stable LatentMoE framework with 896 N |
| Qwen3-235B-A22B | Open-weighted (Released) | Total Parameters: 235 billion; Activated Parameters: 22 billion |
| Qwen3-30B-A3B | Open-weighted (Released) | Total Parameters: 30 billion; Activated Parameters: 3 billion |
| Qwen3.8-Flash-Next | Released as open-weight on August 26, 2026 | Multimodal MoE; Architecture preview for Qwen4 |
| kimi-k2.5 | retired | Visual understanding, code reproduction, 'Agent cluster' collaboration; all-around office assistant. |
| Kimi K3.1 Pro | Released (announced) | [About 3 trillion parameters; supports a context window of 1 million tokens] |
| Kimi K3.1 Pro Highspeed | Released alongside main model | 2-3x the speed of K3.1 Pro |
| Qwen 4 | In training, no public release date announced (as of 9/22/2026) | Architecture preview available via Qwen3.8-Flash-Next |
| DeepSeek-V3 | released | 671B total parameters, 37B activated params per token, MoE architecture |
| DeepSeek V3.5 | Not confirmed / Unreleased (Official changelog shows next step is the 'V4 family' in April 2026) | $0.15 per million input tokens, $0.60 per million output tokens |
| kimi-for-coding | Released | Input: Text, images, and video; Output: Text; Context up to 1048576 tokens; Reasoning effort Low/High/Max |
| Kimi-K2.6 | Released (2026-04-21) | New breakthrough in long-task and agent capabilities. |
| kimi-k2.7-code | deprecated (as of May 25, 2026 for new users; fully retired by end of Dec 2026) | High speed version; output speed is 5 to 6 times that of the standard version; price is 2x |
| DeepSeek-V5 | planned (no V5 id exists yet) | Reported at ~2 trillion parameters; planned to train on Huawei Ascend chips (Q4 2026); open weights pattern not yet for |
| deepseek-flash | Served by DeepSeek-V4.1-Flash-1017 |  |
| DeepSeek-V3.2 | Released (upgraded from deepseek-chat/reasoner) |  |
| DeepSeek-V4-Flash | live and open-sourced |  |
| K2.8 Preview | Fully rolled out in Kimi Code |  |
| K3.1 Pro | Rolled out in Kimi Code as first model in K3.1 series |  |
| moonshot-v1 series models (including -vision-preview and moonshot-v1-auto) | retired |  |

Not independently confirmed: DeepSeek V4 Flash (0731), K2.8 Preview, kimi-for-coding, Kimi-K2.6, moonshot-v1 series models (including -vision-preview and moonshot-v1-auto), Qwen3-235B-A22B, Qwen3-30B-A3B.

39 more named in the sources, with no details or below the first 25: in the project file.

## Checks

- 24 cited sentences re-read against the pages: 14 supported, 4 partly supported, 6 removed.
- 3 claims could not be checked and are not in this report.

## References

[1] Kimi K3.1 Pro：智能前沿，更进一步 - https://www.kimi.com/news/kimi-k3-1-pro (2026-10-24T04:00:00.000Z)
[2] Model lifecycle and updates - Alibaba Cloud Model Studio - https://docs.modelstudio.console.alibabacloud.com/en/model-studio/newly-released-models
[3] DeepSeek V4 Flash and Pro now available on Workers AI · Changelog - https://developers.cloudflare.com/changelog/post/2026-08-14-deepseek-v4-workers-ai/ (2026-08-14T00:00:00.000Z)
[4] DeepSeek V4.1 Flash 1017 now available on Workers AI · Changelog - https://developers.cloudflare.com/changelog/post/2026-10-19-deepseek-v4.1-flash-workers-ai/
[5] Qwen/Qwen3.8-Flash-Next · Hugging Face - https://huggingface.co/Qwen/Qwen3.8-Flash-Next
[6] GitHub - QwenLM/Qwen3.8-Flash-Next: Qwen3.8-Flash-Next is the foundation model developed by Qwen Team, Alibaba Group. · GitHub - https://github.com/QwenLM/Qwen3.8-Flash-Next/
[7] Qwen 3.8 Flash-Next vs Qwen 3.8 27B: Which Model Should You Use? | MyClaw.ai - https://myclaw.ai/blog/qwen-3-8-flash-next-vs-qwen-3-8-27b
[8] Qwen4’s architecture is here early, firing 6B parameters out of 125B - https://thenextweb.com/news/qwen38-flash-next-qwen4-architecture-open-licence-ai-act (2026-08-26T00:00:00.000Z)
[9] transformers/docs/source/en/model_doc/qwen4_exp.md at main · huggingface/transformers · GitHub - https://github.com/huggingface/transformers/blob/main/docs/source/en/model_doc/qwen4_exp.md
[10] DeepSeek Versions — every DeepSeek model from DeepSeek-LLM through V4, with HuggingFace ids, architecture notes, and license terms - https://mungomash.com/ai/deepseek/versions/ (2026-09-10)
[11] DeepSeek Models: V4.1 Flash, V4 Pro, R1 & V3.2 Compared (October 2026) | BenchLM.ai - https://benchlm.ai/providers/deepseek
[12] Change Log | DeepSeek API Docs - https://api-docs.deepseek.com/updates/
[13] DeepSeek AI Blog (2026) — News, Guides & Model Updates - https://deepseek.ai/blog (2026-08-20)
[14] Your First API Call | DeepSeek API Docs - https://api-docs.deepseek.com/updates?_cb=1791871858766 (2026-10-17)
[15] Where the DeepSeek API Stands on October 22, 2026 - https://shattered.io/where-the-deepseek-api-stands-on-october-22-2026/ (2026-10-22T10:26:51.000Z)
[16] Kimi K3 Tech Blog: Open Frontier Intelligence - https://www.kimi.ai/blog/kimi-k3
[17] Model Configuration | Kimi Code Docs - https://www.kimi.com/code/docs/en/kimi-code/models.html
[18] Moonshot AI Launches More Powerful Kimi Model - Caixin Global - https://www.caixinglobal.com/2026-10-26/moonshot-ai-launches-more-powerful-kimi-model-102497310.html (2026-10-26)
[19] DeepSeek V4.5: The Open-Weight Model That Undercuts Everyone — Swfte AI - https://www.swfte.com/blog/deepseek-v4-5-open-weight-deep-dive-2026 (2026-07-05T00:00:00.000Z)
[20] Alibaba: Qwen3.8 Flash-Next - API Pricing & Benchmarks - https://opper.ai/alibaba/qwen3-8-flash-next
[21] Qwen/Qwen3.8-Flash-Next | vLLM Recipes - https://recipes.vllm.ai/Qwen/Qwen3.8-Flash-Next (2026-10-13)
[22] deepseek-v4-flash-0731 (DeepSeek) · Cloudflare AI docs · Cloudflare Workers AI docs - https://developers.cloudflare.com/workers-ai/models/deepseek-v4-flash-0731/
[23] deepseek-v4-pro-0813 (DeepSeek) · Cloudflare AI docs · Cloudflare Workers AI docs - https://developers.cloudflare.com/workers-ai/models/deepseek-v4-pro-0813/

# Kimi K2.6 technical report

This report describes Kimi K2.6, a mixture-of-experts language model. K2.6 improves long-context retrieval over K2.5 and reaches a score of 71.2 on the long-document question answering benchmark described in Section 4.

## Model

Kimi K2.6 has 1 trillion total parameters, of which 32 billion are active for each token. The context window is 256k tokens. Training used a curriculum that increases sequence length in stages.

## Results

On long-document question answering the model improves by 6.4 points over Kimi K2.5. On agentic coding tasks the improvement is smaller. We release the weights so that others can reproduce the numbers.

## Limitations

The model can still lose track of details in the middle of very long inputs, and its multilingual quality outside Chinese and English has not been evaluated as thoroughly.

# V-2 验证器自测向量

- `anchor_vector_v2.json`：evorule-governance ≥0.8.2（evorule-anchor/2）真实生成的双锚点链
- `anchor_vector_v2_pubkey.json`：配套验签公钥（seed=ab×32 派生，测试用）

## 自测（零依赖跑通）

```
python tools/verify_anchors.py   --atif <(echo '{"extra":{}}')   --anchors tools/testdata/anchor_vector_v2.json   --pubkey $(python -c "import json;print(json.load(open('tools/testdata/anchor_vector_v2_pubkey.json'))['pubkey'])")
```

预期：T2/T3/T1 全 PASS。

生成源：evorule-anchor 仓 `cargo run -p evorule-governance --example anchor_vector`

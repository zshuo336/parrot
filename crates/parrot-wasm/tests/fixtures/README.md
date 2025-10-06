# C 阶段测试 fixture

`fixture-component.wasm`：实现 `wit/parrot-actor.wit` 冻结世界的示例组件
（echo/up/self/config/clock/log/err/spin/state/counter/started/drained + tell bump）。

重建（源码在 `../fixture/`——独立 workspace，wasm32-unknown-unknown 目标）：

```bash
cd ../fixture
cargo build --target wasm32-unknown-unknown --release
wasm-tools component new target/wasm32-unknown-unknown/release/parrot_wasm_fixture.wasm -o ../fixtures/fixture-component.wasm
```

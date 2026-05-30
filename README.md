# hyperbot

合约（永续）网格交易 Bot，基于 [hyperliquid-rust-sdk](https://github.com/hyperliquid-dex/hyperliquid-rust-sdk)。

当前默认策略配置：

- 交易对：**XMR/USDC 永续**（SDK 中 coin 符号为 `XMR`）
- 网格类型：**等差网格（arithmetic）**
- 持仓方向：**单边只做空（short_only）**
- 数据库：**PostgreSQL + `sqlx`**（迁移内嵌于二进制，启动时自动执行）

> ⚠️ 风险提示：本项目用于学习与研究。合约交易具有高风险，请务必先在 **Hyperliquid 测试网** 上验证，使用专用 API 钱包，并设置合理的风控参数。

## 架构

| 模块 | 说明 |
| --- | --- |
| `config` | 从 `config.toml` 加载配置，密钥/DSN 由环境变量注入并校验 |
| `telemetry` | 基于 `tracing` 的结构化日志 |
| `exchange` | `Exchange` trait 抽象交易所；`HyperliquidExchange` 实盘实现 + `MockExchange` 测试实现 |
| `grid` | 纯逻辑网格策略（无 I/O，完整单测覆盖） |
| `store` | `sqlx` + PostgreSQL 持久化层（网格订单、成交、持仓快照、运行状态） |
| `risk` | 风控（最大持仓 / 最大亏损 / 杠杆上限，熔断停机） |
| `bot` | 事件循环编排：设杠杆 → 启动对账 → 播单 → 处理成交 → 风控 → 优雅退出 |

### 只做空网格逻辑

1. 启动时，在当前中间价**上方**的每条网格线挂**卖单**（开空）。
2. 某条卖单成交（开空）后，在其**下一档**挂 `reduce_only` 的**买单**（止盈平空）。
3. 该买单成交（平空获利）后，在原档位**重新挂卖单**。

因此净持仓恒为 ≤ 0（只做空，单边持仓）；所有买单均为只减仓。

## 快速开始（本地）

```bash
cp config.example.toml config.toml      # 按需修改网格/风控参数
cp .env.example .env                     # 填入私钥、数据库等
export $(grep -v '^#' .env | xargs)      # 或使用 direnv
cargo run --bin hyperbot
```

需要可访问的 PostgreSQL，并通过 `DATABASE_URL` 指定，例如：
`******localhost:5432/hyperbot`。

## Docker Compose 一键启动

```bash
cp config.example.toml config.toml
cp .env.example .env                     # 至少填入 HYPERBOT_PRIVATE_KEY
docker compose up -d --build
```

`docker-compose.yml` 会启动：

- `postgres`：PostgreSQL 16，数据持久化于命名卷 `pgdata`，带健康检查。
- `bot`：网格 Bot，待数据库健康后启动；`config.toml` 以只读方式挂载，密钥经环境变量注入。

## 配置

完整示例见 [`config.example.toml`](config.example.toml)。敏感字段从环境变量注入，**不要**写进文件或镜像：

| 环境变量 | 含义 |
| --- | --- |
| `HYPERBOT_PRIVATE_KEY` | API 钱包私钥（hex） |
| `DATABASE_URL` | PostgreSQL 连接串 |
| `HYPERBOT_NETWORK` | 覆盖网络：`mainnet` / `testnet` |
| `HYPERBOT_CONFIG` | 配置文件路径（默认 `config.toml`） |
| `RUST_LOG` | 日志级别（默认 `info`） |

## 开发

```bash
make build          # 编译
make test           # 单元测试（无需数据库）
make clippy         # 静态检查（-D warnings）
make fmt            # 格式化

# 集成测试需要一个可写的 PostgreSQL：
TEST_DATABASE_URL=postgres://postgres@localhost:5432/hyperbot \
  cargo test --test grid_flow
```

数据库迁移脚本位于 [`migrations/`](migrations/)，使用 `sqlx::migrate!` 内嵌并在启动时自动执行。

## 许可证

MIT

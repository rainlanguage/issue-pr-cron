# Org / repo registry

Regenerate: loop
`gh repo list <org> --json name,description,pushedAt,isArchived` over the orgs
below. Orgs the account belongs to but that are dormant/out-of-scope:
godel-design, FurstMedia, hoplon, travelling-tradies, hummhive, polygen-io,
Polinate-io, dragonflysun.

Org cheat-sheet:

- rainlanguage — the main org: rain language, raindex, words repos, rainix CI,
  cron pipeline targets
- cyclofinance — Cyclo product (cysFLR): cyclo.sol contracts, cyclo.site
  frontend (has its OWN strategy .rain files + live price fetching), rewards,
  subgraph
- S01-Issuer — st0x.deploy (audited prod deploys, V4 campaign)
- ST0x-Technology — st0x adjacent
- h20liquidity — legacy strategy/liquidity work
- gildlab — infra/home org
- raincommercial / rain-archive — commercial + archived rain work

## rainlanguage

rain.strategies | 2026-07-04 | rain.erc4626.words | 2026-07-04 | rainix |
2026-07-04 | Nix derivations for common rain automations and development
environment configuration. rain.flare | 2026-07-03 | Rainy integration with
Flare Network. raindex | 2026-07-03 | Rain orderbook libraries, subgraph and
contract implementation. rain.math.float | 2026-07-03 | An implementation of
decimal floating point math in Solidity (mostly yul assembly). rainlang.xyz |
2026-07-03 | rainlang | 2026-07-03 | Solidity library for implementing Rain
compatible interpreters. raindex.interface | 2026-07-03 | Interfaces for the
rain orderbook. rain.devops | 2026-07-03 | flow | 2026-07-03 | Solidity
interfaces for flow contracts orderbook-subgraph (ARCHIVED 2026-07-04) |
2026-07-03 | raindex.governance | 2026-07-01 | Ownership and access-control
tooling for Raindex orders and vaults (RaindexInventory: one shared vault pool,
many authorised operators). sqlite-web | 2026-07-01 | dotrain | 2026-06-29 |
.rain to rainlang composer and rain language server protocol services
rain.deploy | 2026-06-29 | Tools for deploying Solidity code and checking
existing deployments to supported networks. rain.solver | 2026-06-28 | Node.js
app that solves (clears) Rain Orderbook orders against onchain liquidity (DEXes,
other Rain Orderbooks and orders) issue-pr-cron | 2026-06-28 | Autonomous GitHub
issue→PR cron for the rainlanguage org — opens fix PRs for open issues; never
merges, deploys, or closes issues. rain-org-health | 2026-06-28 | Claude Code
marketplace: rain-org-health-check skill — audit rainlanguage repos for
rainix/soldeer modernization debt base-node | 2026-06-27 | Everything required
to run your own Base node rain.webapp | 2026-06-23 | specs | 2026-06-22 |
rain.vats | 2026-06-21 | Verifiable Asset Tokenization System (VATS)
rain.factory | 2026-06-20 | Solidity interfaces and implementation of Rain
factories. rain.metadata | 2026-06-19 | Contracts, libs, and tooling for Rain
metadata rain.math.fixedpoint | 2026-06-19 | 18 decimal fixed point math in
solidity github-chore | 2026-06-18 | alloy-ethers-typecast (ARCHIVED 2026-07-04,
unused) | 2026-06-17 | claude-audit-skills | 2026-06-16 |
adversarial-mutation-test | 2026-06-16 | Claude Code plugin: build or harden a
repo's test suite so every test provably covers code (adversarial mutation
testing, whole-repo, resumable). pyth-crosschain | 2026-06-16 | Crosschain Pyth
programs and utilities rain.tofu.erc20-decimals | 2026-06-15 | TOFU lookups for
erc20 decimals sushiswap | 2026-06-15 | Sushi 2.0 🍣 rain.extrospection |
2026-06-15 | rain.interpreter.interface | 2026-06-15 | rain.lib.memkv |
2026-06-15 | An in-memory KV store (hashmap) implemented in Solidity (mostly yul
assembly). rain.erc | 2026-06-15 | rain.datacontract | 2026-06-15 | SSTORE2
inspired implementation of data contracts rain.wasm | 2026-06-15 | Provides wasm
bindgen utilities and helpers docs.rainlang.xyz | 2026-06-15 | rain.dia |
2026-06-15 | private-key-dev-recovery | 2026-06-06 | TEMPORARY: admin-gated
recovery of PRIVATE_KEY_DEV. Delete after use. dvin.deploy | 2026-06-05 |
rain.uniswap | 2026-06-05 | ARCHIVED | Uniswap for rainlang. Words and libs.
rain.subgraph.docker | 2026-06-05 | Shared environment for testing subgraphs
rainlang-vscode | 2026-06-05 | ARCHIVED | Rain language implementation for
vscode rainlang-codemirror | 2026-06-05 | Rain language implementation for
codemirror text editor rain.tier.interface | 2026-06-05 | ITierVX interfaces for
rain rain.chainlink | 2026-06-05 | Implementation of chainlink oracles for rain.
router | 2026-06-05 | rain.verify | 2026-06-05 | rain.pyth | 2026-06-04 |
Exposes the pyth oracles to rainlang expressions rain.verify.interface |
2026-06-04 | rain.solmem | 2026-06-04 | Solidity lib for working with bytes in
memory rain.sol.codegen | 2026-06-04 | Solidity that writes solidity.
rain.sol.binmaskflag | 2026-06-04 | Binary masks and flags in Solidity as simple
constants instead of runtime compute. rain.math.saturating | 2026-06-04 |
Solidity library containing saturating math logic. rain.math.binary | 2026-06-04
| Binary math in solidity. rain.lib.typecast | 2026-06-04 | Solidity library for
casting and coercing types rain.lib.hash | 2026-06-04 | Solidity library for
hashing without abi.encode (avoiding allocation)

## cyclofinance

cyclo.sol | 2026-05-09 | Solidity contracts for https://cyclo.finance cyclo.site
| 2026-05-07 | Front end for Cyclo rflr-nix (ARCHIVED 2026-07-04) | 2026-04-23 |
cyclo.rewards | 2026-04-22 | cyclo.subgraph | 2026-04-20 | dimension-adapters |
2025-04-28 | DefiLlama-Adapters | 2025-04-27 | cyclo.rewardsold | 2025-01-30 |
ARCHIVED | cyclo.brand | 2024-11-13 |

## S01-Issuer

st0x.deploy | 2026-07-04 | s01.ops | 2026-07-04 | S01 Issuer GmbH operational
platform. SvelteKit + Tailwind + Flowbite. st0x.atomic-bridge | 2026-07-03 |
Per-ticker atomic-bridge contracts: Morpho Blue as the
collateral/IRM/liquidation primitive; permissioned IOU + gateway + oracle
collapsed into one proxy per ticker, behind a beacon for global atomic upgrades.
Albion-issuance-site | 2026-06-11 | Albion website and royalty token platform
frontend. Users can learn about Albion and connect their wallet to mint and
manage their royalty tokens and payment tokens. st0x-timelock-deploy |
2026-04-08 | sft-ownership-transfer | 2026-04-08 | st0x-tokens | 2026-03-30 |
ARCHIVED | s1-issuer | 2026-03-14 | s01.agent | 2026-03-12 | Agent skills for
S01 Issuer S01-kb | 2026-03-04 | David / Nick Agents st0x-rewards-script |
2026-01-14 | st0x-rewards | 2026-01-08 | landing-page | 2025-10-06 |

## ST0x-Technology

st0x.devops | 2026-07-04 | st0x.liquidity | 2026-07-04 | st0x.issuance |
2026-07-04 | st0x-oracle-server | 2026-07-03 | st0x.univ4.hook | 2026-07-03 |
Uniswap v4 PropAMM hook for ST0x tokenized equities turnkey-policy-spec |
2026-07-03 | st0x.raindex-deploy | 2026-07-03 | st0x.rest.api | 2026-07-03 |
ST0x REST API event-sorcery | 2026-07-03 | Event sourcing but with type-level
magic to prevent shooting yourself in the foot. notion-content | 2026-07-03 |
Notion workspace mirror for st0x - auto-synced every 30 minutes st0x-marketing |
2026-07-02 | ST0x growth system — hypotheses, experiments, content,
partnerships, market intelligence st0x.bebop | 2026-07-02 | st0x.quant |
2026-07-01 | Quantitative analysis and market making models
st0x.liquidity-monitor | 2026-07-01 | st0x.dividend.processes | 2026-07-01 |
ST0x tokenised dividend ops CLI on Base: snapshot, Merkle distribution, and
wrapper-vault management st0x.registry | 2026-06-30 | st0x.oracle | 2026-06-30 |
st0x.observability | 2026-06-26 | Prometheus + Grafana + Loki + Alertmanager for
st0x services st0x.pricing | 2026-06-25 | st0x.pricing-types | 2026-06-22 |
Shared wire types for the st0x.pricing service and its consumers (CBOR over
WebSocket, Rain Floats as 32-byte byte strings). st0x.incentives | 2026-06-16 |
st0x.docs | 2026-06-16 | Public-facing product documentation for ST0x (synced to
GitBook) sft-ops | 2026-06-10 | st0x.alerts | 2026-06-02 | bd-agent | 2026-04-15
| ST0x-intelligence | 2026-04-15 | NM-old-copy-please-delete | 2026-03-29 |
Red - Nick pipeline intelligence agent investor-agent | 2026-03-25 | ST0x
onchain investor re-engagement agent .github | 2026-02-12 |

## h20liquidity

sft-tokenisation | 2026-06-19 | nht-comliq | 2026-06-04 | mnw-comliq |
2025-09-25 | wlth-comliq | 2025-09-25 | h20-site | 2025-09-16 | ioen-script |
2025-09-07 | quantum-fx | 2025-08-21 | rain-model-validation | 2025-05-05 |
Raindex strategy model - trade count between resets - validation scripts
sarco-comliq | 2025-04-22 | h20.customerstrats | 2024-10-17 | h20.test-std |
2024-09-06 | h20.communitystrats | 2024-08-19 | Community strategies to share
raindex.webhook | 2024-07-27 | h20.redblue | 2024-06-14 | RED and BLUE tokens.
.github | 2024-05-27 | h20.demo.signedclaims | 2024-05-09 | demo-repository |
2024-04-29 | A code repository designed to show the best GitHub has to offer.
orderbook.test.utils | 2024-04-09 | bj | 2024-04-04 | arb-bot-mev-share |
2024-03-28 | zentu | 2024-03-28 | tft | 2024-03-26 | ieon | 2024-03-12 | dhb |
2024-03-07 | xblock | 2024-03-06 | flare-oracle-strategy | 2024-02-16 |
polytrade | 2024-02-15 | neighbourhoods | 2024-01-09 | love-to-front-end |
2023-10-30 | block-scanner | 2023-10-24 | common-wealth-strategies | 2023-10-14
| zeroex-take-order-bot | 2023-06-10 | A bot running on NodeJs for targeting
specified Rain orderbook orders to clear them against 0x liquidity
ob-dispair-deploy | 2023-04-27 | ruby1 | 2023-04-06 |

## gildlab

upptime | 2026-07-04 | 📈 Uptime monitor and status page for gildlab, powered by
@upptime offchainAssetVault-subgraph | 2026-06-29 | Subgraph for
OffchainAssetVault contract gildlab.cli | 2024-10-28 | CLI toolkit for gildlab.
SFT | 2024-10-28 | ipfs-node | 2024-06-23 | server scripts .github | 2024-05-27
| private-issues | 2024-05-20 | gildlab-website | 2023-11-20 | interwoven |
2023-10-25 | user-docs | 2023-06-23 | specs | 2023-06-13 | sloshy | 2023-02-01 |
wiki.gildlab.xyz | 2022-12-18 | ETHg-subgraph | 2022-09-07 | ethgild-interface |
2022-08-04 | sft-subgraph | 2022-07-11 | subgraph | 2021-12-16 |

## raincommercial

billboard-flows | 2023-12-06 | rain-oxits | 2023-09-02 | oxits-claim-site |
2023-06-25 | rain-rules-marketplace | 2023-06-14 | RainGameEngine-Backend |
2022-11-08 | subgraph-health-checker | 2022-10-11 | A cron job server for
cheking subgraph health and reporting to the telegram rain-game-engine-front-end
| 2022-09-13 | rain-game-sdk | 2022-09-13 | rain-game-marketplace-poc |
2022-08-17 | rain-ui-components | 2022-07-16 | hume721a | 2022-06-01 | Hume721A

## rain-archive

rain-chess | 2022-11-14 | LiChess | 2022-11-08 | ERC1155NFT | 2022-02-01 |
v2-subgraph | 2022-01-14 | Subgraph for new version of rain protocol sstore2 |
2021-12-19 | Faster & cheaper contract key-value storage for Ethereum Contracts
balancer-core | 2021-09-21 | configurable-rights-pool | 2021-09-21 |

# clipx 性能基线一键跑（对应 docs/listary-reference.md §5.6）
# 用法：pwsh scripts/perf.ps1 [-Rounds 9] [-Scales "1000,5000,20000"]
# 基线必须用 --release 跑；结果贴进 docs/perf-baseline.md。
param(
    [int]$Rounds = 9,
    [string]$Scales = "1000,5000,20000"
)
$ErrorActionPreference = "Stop"
$env:ROUNDS = "$Rounds"
$env:SCALES = $Scales
cargo run --release -p clipx-store --example perf_suite

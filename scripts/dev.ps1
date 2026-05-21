# Development helper for Windows PowerShell
Write-Host "Checking Rust workspace..." -ForegroundColor Cyan
cargo check
if ($LASTEXITCODE -ne 0) {
    Write-Error "Cargo check failed!"
    exit 1
}

Write-Host "Running tests..." -ForegroundColor Cyan
cargo test
if ($LASTEXITCODE -ne 0) {
    Write-Error "Cargo test failed!"
    exit 1
}

Write-Host "Building dev workspace..." -ForegroundColor Green
cargo build

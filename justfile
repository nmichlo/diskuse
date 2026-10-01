# everything CI checks, in one command
check:
    uvx --from pre-commit pre-commit run --all-files
    cargo test
    cargo deny check

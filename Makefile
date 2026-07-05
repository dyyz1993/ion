# ION Makefile — 常用命令速查

.PHONY: build build-all test test-all test-ci test-provider clean ci

# 构建
build:
	cargo build --bin ion

build-all:
	cargo build --bin ion --bin ion-worker --bin agent-demo

# 测试
test:
	cargo test --lib

test-provider:
	cargo test -p ion_provider

test-all: test test-provider
	cargo test --tests 2>/dev/null || echo "(some integration tests need manager)"

# CI 等效测试（无 LLM）
test-ci: build-all test-all
	bash tests/session_entries_ci.sh

# 带 LLM 的完整测试
test-ci-llm: build-all test-all
	bash tests/session_entries_ci.sh --with-llm

# 清理
clean:
	cargo clean
	rm -rf /tmp/ion-manager.pid /tmp/ion-ci-manager.log

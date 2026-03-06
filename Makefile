.PHONY: all build release agent clean test

all: build agent

# Build the Rust binary (debug)
build:
	cargo build

# Build the Rust binary (release)
release:
	cargo build --release
	cp ym-agent/ym-agent.jar target/release/ 2>/dev/null || true

# Build the Java agent
agent:
	@mkdir -p ym-agent/out
	javac -d ym-agent/out ym-agent/src/main/java/dev/yummy/agent/*.java
	jar cfm ym-agent/ym-agent.jar ym-agent/src/main/resources/META-INF/MANIFEST.MF -C ym-agent/out .
	cp ym-agent/ym-agent.jar target/debug/ 2>/dev/null || true
	cp ym-agent/ym-agent.jar target/release/ 2>/dev/null || true
	@echo "Built ym-agent.jar"

# Run tests
test: build
	cargo test

# Clean all build artifacts
clean:
	cargo clean
	rm -rf ym-agent/out ym-agent/ym-agent.jar
	rm -rf ym-ecj-service/out ym-ecj-service/ym-ecj-service.jar

# Install to ~/.local/bin
install: release agent
	cp target/release/ym ~/.local/bin/
	cp ym-agent/ym-agent.jar ~/.local/bin/
	@echo "Installed ym to ~/.local/bin/"

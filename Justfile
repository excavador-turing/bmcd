# What CI runs, runnable here.
#
# Every recipe below is the exact command from .github/workflows/cargo_ci.yml.
# That is the point: a recipe that only approximates CI tells you nothing when
# it passes. If CI changes, these change with it.
#
# The toolchain is pinned to 1.85.0 to match that workflow, NOT the 1.98.1 the
# firmware builds with. They differ on purpose: clippy gains lints every
# release and `-D warnings` would turn this red without anyone touching the
# code. devbox.json installs the pinned one.

default:
    @just --list

# Everything CI checks, in CI's order — run before opening a pull request
check: fmt clippy build test clippy-stubbed test-stubbed

# Formatting, as CI checks it
fmt:
    cargo fmt --all --check

# Reformat in place; `check` is what gates, this is what fixes
fix:
    cargo fmt --all

# Lints, denied as warnings
clippy:
    cargo clippy --workspace --all-targets --locked -- -D warnings

# Build every target from the committed lockfile
build:
    cargo build --workspace --all-targets --locked

# Unit tests
test:
    cargo test --workspace --locked

# The `stubbed` feature swaps the GPIO/sysfs HAL for an in-memory simulation and
# is the only way to build this daemon without a Turing Pi under it. It had not
# compiled since before v2.3.7 because nothing built it, so it drifted out of
# sync with the HAL it stands in for and nobody found out. These two are what
# stop that happening again.

# Lints against the hardware-less HAL
clippy-stubbed:
    cargo clippy --workspace --all-targets --features stubbed --locked -- -D warnings

# Tests against the hardware-less HAL
test-stubbed:
    cargo test --workspace --features stubbed --locked

# Licences, bans and sources, as the second CI job checks them
deny:
    cargo deny check bans licenses sources

# bmcd cannot be flashed on its own — see tp2-bmc-firmware's `just dev-flash`
flash:
    @echo "bmcd is cross-compiled into the firmware image by Buildroot, against"
    @echo "that image's toolchain and sysroot -- it is not installed on its own."
    @echo
    @echo "To try this tree on a board, from a tp2-bmc-firmware checkout:"
    @echo "    just dev-flash --bmcd $(pwd)"
    @exit 1

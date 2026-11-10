#!/usr/bin/env just --justfile

main_crate := 'osmflow'
# How to call the current just executable. Note that just_executable() may have `\` in Windows paths, so we need to quote it.
just := quote(just_executable())
# cargo-binstall needs a workaround due to caching when used in CI
binstall_args := if env('CI', '') != '' {'--no-confirm --no-track --disable-telemetry'} else {''}
# location of the coverage output, used by CI
coverage_lcov := 'target/llvm-cov/lcov.info'
# location of the jscpd copy/paste detection reports, used by CI
cpd_output := 'target/jscpd'
# Where `dist` puts the release asset: osmflow-<target>.tar.gz, or .zip for Windows, with the binary and the licenses,
# as crates/osmflow-cli/Cargo.toml's binstall metadata expects
dist_dir := 'target/dist'
dist_build := 'cargo build --release --locked --package osmflow-cli --bin osmflow --target'

# if running in CI, treat warnings as errors by setting CARGO_BUILD_WARNINGS to 'deny' unless it is already set
# Use `CI=true just ci-test` to run the same tests as in GitHub CI.
# Use `just env-info` to see the current value of CARGO_BUILD_WARNINGS
ci_mode := if env('CI', '') != '' {'1'} else {''}
export CARGO_BUILD_WARNINGS := env('CARGO_BUILD_WARNINGS', if ci_mode == '1' {'deny'} else {'warn'})
export RUST_BACKTRACE := env('RUST_BACKTRACE', if ci_mode == '1' {'1'} else {'0'})

@_default:
    {{just}} --list

# Time lookups and commits of the node stores
bench:
    cargo bench -p osmflow --bench nodes

# Time PBF readers on OSMFLOW_BENCH_PBF (benches/README.md), each package built on its own so their features never merge
bench-pbf *packages='osmflow osmpbf osmpbfreader':
    for p in {{packages}}; do cargo bench -p "osm-bench-$p" || exit 1; done

# Build the project
build:
    cargo build --workspace --all-features --all-targets

# Quick compile without building a binary
check:
    cargo check --workspace --all-features --all-targets

# Generate LCOV coverage report for CI to upload to codecov.io
ci-coverage: env-info && \
        (_coverage '--lcov' '--output-path' quote(coverage_lcov))
    rm -rf {{quote(parent_directory(coverage_lcov))}}
    mkdir -p {{quote(parent_directory(coverage_lcov))}}

# Find copy/pasted code, marking clones absent from base_ref as new, and write a Markdown summary for the PR comment
ci-cpd base_ref='origin/main':  (assert-cmd 'jq') (cpd '--reporters' 'console,json' '--output' cpd_output '--baseline-from-ref' base_ref)
    jq -r --arg base {{quote(base_ref)}} -f .github/jscpd-summary.jq {{quote(cpd_output / 'jscpd-report.json')}} > {{quote(cpd_output / 'summary.md')}}
    cat {{quote(cpd_output / 'summary.md')}} >> "${GITHUB_STEP_SUMMARY:-/dev/stdout}"

# Run all tests as expected by CI
ci-test: env-info codegen-check test-fmt clippy test test-doc && assert-git-is-clean

# Compile default features with minimal dependencies on the configured MSRV
ci-test-msrv:
    {{just}} ci_mode=0 env-info _check-msrv-default
    {{just}} assert-git-is-clean

# Set toolchain and run ci-test-msrv
ci-test-msrv-with-toolchain:
    RUSTUP_TOOLCHAIN="$({{just}} get-msrv)" {{just}} ci-test-msrv

# Clean all build artifacts
clean:
    cargo clean
    rm -f Cargo.lock

# Run cargo clippy to lint each package with all its features, with none, and with each one alone
clippy *args:  (cargo-install 'cargo-hack')
    cargo hack clippy --workspace --each-feature --all-targets {{args}}

# Generate and open the HTML coverage report
coverage:  (_coverage '--open')

# Clean, collect, and aggregate coverage using the requested report arguments
_coverage *report_args:  (cargo-install 'cargo-llvm-cov')
    cargo llvm-cov clean --workspace
    cargo llvm-cov --no-report --workspace --all-features --all-targets
    cargo llvm-cov report --include-build-script {{report_args}}

# Find copy/pasted code with jscpd, configured in .jscpd.json. See `just cpd --help` for all options
cpd *args:  (cargo-install 'jscpd' 'jscpd --bin jscpd' '--bin-dir={bin}{binary-ext}')
    jscpd {{args}}

# Build osmflow-cli's release binary for `target`, on Linux or macOS, and pack it as the release asset in `target/dist`
[unix]
dist target:
    rustup target add {{quote(target)}}
    {{dist_build}} {{quote(target)}}
    rm -rf {{quote(dist_dir / 'osmflow-' + target)}}* && mkdir -p {{quote(dist_dir / 'osmflow-' + target)}}
    cp {{quote('target' / target / 'release/osmflow')}} LICENSE-MIT LICENSE-APACHE {{quote(dist_dir / 'osmflow-' + target)}}/
    cd {{quote(dist_dir / 'osmflow-' + target)}} && tar acf {{quote('../osmflow-' + target + '.tar.gz')}} osmflow LICENSE-MIT LICENSE-APACHE

# Build osmflow-cli's release binary for `target`, on Windows, and pack it as the release asset in `target/dist`, with
# the C runtime linked in, so the binary needs no Visual C++ redistributable
[windows]
dist target:
    rustup target add {{quote(target)}}
    RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static" {{dist_build}} {{quote(target)}}
    rm -rf {{quote(dist_dir / 'osmflow-' + target)}}* && mkdir -p {{quote(dist_dir / 'osmflow-' + target)}}
    cp {{quote('target' / target / 'release/osmflow.exe')}} LICENSE-MIT LICENSE-APACHE {{quote(dist_dir / 'osmflow-' + target)}}/
    cd {{quote(dist_dir / 'osmflow-' + target)}} && 7z a {{quote('../osmflow-' + target + '.zip')}} osmflow.exe LICENSE-MIT LICENSE-APACHE

# Build and open code documentation
docs *args='--open':
    DOCS_RS=1 cargo doc --no-deps {{args}} --workspace --all-features

# Regenerate osmflow's protobuf code from its .proto files (needs protoc)
update-generated:
    cargo run -p osmflow --example codegen

# Regenerate osmflow's protobuf code, and fail if the checked-in code changes
codegen-check: update-generated assert-git-is-clean

# Print environment info
env-info:
    @echo "Running for '{{main_crate}}' crate {{if ci_mode == '1' {'in CI mode'} else {'in dev mode'} }} on {{os()}} / {{arch()}}"
    @echo "PWD {{justfile_directory()}}"
    {{just}} --version
    rustc --version
    cargo --version
    rustup --version
    @echo "CARGO_BUILD_WARNINGS='$CARGO_BUILD_WARNINGS'"
    @echo "RUST_BACKTRACE='$RUST_BACKTRACE'"

# Reformat all code `cargo fmt`. If nightly is available, use it for better results
fmt:
    #!/usr/bin/env bash
    set -euo pipefail
    if (rustup toolchain list | grep nightly && rustup component list --toolchain nightly | grep rustfmt) &> /dev/null; then
        echo 'Reformatting Rust code using nightly Rust fmt to sort imports'
        cargo +nightly fmt --all -- --config imports_granularity=Module,group_imports=StdExternalCrate
    else
        echo 'Reformatting Rust with the stable cargo fmt.  Install nightly with `rustup install nightly` for better results'
        cargo fmt --all
    fi

# Reformat all Cargo.toml files using cargo-sort
fmt-toml *args:  (cargo-install 'cargo-sort')
    cargo sort --workspace --grouped {{args}}

# Get a package field from the metadata
get-crate-field field package=main_crate:  (assert-cmd 'jq')
    @cargo metadata --no-deps --format-version 1 | jq -e -r '.packages | map(select(.name == "{{package}}")) | first | .{{field}} // error("Field \"{{field}}\" is missing in Cargo.toml for package {{package}}")'

# Get the minimum supported Rust version (MSRV) for the crate
get-msrv package=main_crate:  (get-crate-field 'rust_version' package)

# Find the minimum supported Rust version (MSRV), update Cargo.toml, and test minimal dependencies
msrv:  (cargo-install 'cargo-msrv')
    cargo msrv find --manifest-path crates/{{main_crate}}/Cargo.toml --write-msrv --ignore-lockfile -- {{just}} _check-msrv-default

# Compile the crate's default features using a dynamically generated minimal Cargo.lock
_check-msrv-default:  (cargo-install 'cargo-minimal-versions') (cargo-install 'cargo-hack')
    #!/usr/bin/env bash
    set -euo pipefail
    # cargo-msrv probes with rustup, but nested cargo subcommands may otherwise
    # fall back to the default Cargo and emit flags unsupported by the candidate rustc.
    toolchain="$(rustc --version | cut -d' ' -f2)"
    export RUSTUP_TOOLCHAIN="$toolchain"
    export CARGO="$(rustup which --toolchain "$toolchain" cargo)"
    cargo minimal-versions check --direct --all-features --package {{main_crate}}

# Run release-plz, which CI also runs to publish the crates: `just release update` updates versions and changelogs
release *args='':  (cargo-install 'release-plz')
    release-plz {{args}}

# Run the osmflow command, built in debug mode, with the given arguments: `just run stats dc.osm.pbf`
[positional-arguments]
run *args:
    @cargo run -p osmflow-cli -- "$@"

# Run the osmflow command, built in release mode, with the given arguments: `just run-rel stats dc.osm.pbf`
[positional-arguments]
run-rel *args:
    @cargo run --release --quiet -p osmflow-cli -- "$@"

# Dump a PBF file as text or GeoJSON with the osmflow command: `just dump dc.osm.pbf dc.geojson`
[positional-arguments]
dump *args:
    @{{just}} run-rel dump "$@"

# Summarize a PBF file with the osmflow command: `just stats dc.osm.pbf`
[positional-arguments]
stats *args:
    @{{just}} run-rel stats "$@"

# Find a replication stream's sequence with the osmflow command: `just seq minute-dir 2026-11-09`
[positional-arguments]
seq *args:
    @{{just}} run-rel seq "$@"

# Check semver compatibility with prior published version. Install it with `cargo install cargo-semver-checks`
semver *args:  (cargo-install 'cargo-semver-checks')
    cargo semver-checks --all-features {{args}}

# Run all tests
test:
    # The osmpbf bench builds flate2 with zlib-ng, which flate2 prefers over osmflow's zlib-rs in every crate built with it:
    # it is tested on its own, so that osmflow's tests use the zlib-rs that osmflow's users get
    cargo test --workspace --exclude osm-bench-osmpbf --all-features --all-targets
    cargo test --package osm-bench-osmpbf --all-features --all-targets
    cargo test --doc --workspace --exclude osm-bench-osmpbf --all-features

# Test documentation generation
test-doc:  (docs '')

# Run osmflow-sync's database test against PostGIS in Docker, in the container `osmflow-sync-pg` on port 5434, started
# if it is not running. Its `postgres` database's `osm` schema is replaced.
test-sync-db:  (assert-cmd 'docker')
    docker start osmflow-sync-pg 2>/dev/null || docker run -d --name osmflow-sync-pg -p 5434:5432 -e POSTGRES_HOST_AUTH_METHOD=trust postgis/postgis:18-3.6-alpine
    until docker exec osmflow-sync-pg pg_isready -h localhost -U postgres -q; do sleep 1; done
    OSMFLOW_SYNC_TEST_DB=postgresql://postgres@localhost:5434/postgres cargo test -p osmflow-sync --test sync

# Test code formatting
test-fmt: && (fmt-toml '--check' '--check-format')
    cargo fmt --all -- --check

# Find unused dependencies. Uses `cargo-udeps`
udeps:  (cargo-install 'cargo-udeps')
    cargo +nightly udeps --workspace --all-features --all-targets

# Update all dependencies, including breaking changes. Requires nightly toolchain (install with `rustup install nightly`)
update:
    cargo update

# Ensure that a certain command is available
[private]
assert-cmd command:
    @if ! type {{command}} > /dev/null; then \
        echo "Command '{{command}}' could not be found. Please make sure it has been installed on your computer." ;\
        exit 1 ;\
    fi

# Make sure the git repo has no uncommitted changes
[private]
assert-git-is-clean:
    @if [ -n "$(git status --untracked-files --porcelain)" ]; then \
        >&2 echo "ERROR: git repo is no longer clean. Make sure compilation and tests artifacts are in the .gitignore, and no repo files are modified." ;\
        >&2 echo "######### git status ##########" ;\
        git status ;\
        git --no-pager diff ;\
        exit 1 ;\
    fi

# Check if a certain Cargo command is installed, and install it if needed
[private]
cargo-install $COMMAND $INSTALL_CMD='' $BINSTALL_EXTRA_ARGS='' *args='':
    #!/usr/bin/env bash
    set -euo pipefail
    unset CARGO_BUILD_WARNINGS
    if ! command -v $COMMAND > /dev/null; then
        echo "$COMMAND could not be found. Installing..."
        if ! command -v cargo-binstall > /dev/null; then
            set -x
            cargo install ${INSTALL_CMD:-$COMMAND} --locked {{args}}
            { set +x; } 2>/dev/null
        else
            set -x
            cargo binstall ${INSTALL_CMD:-$COMMAND} {{binstall_args}} $BINSTALL_EXTRA_ARGS --locked
            { set +x; } 2>/dev/null
        fi
    fi

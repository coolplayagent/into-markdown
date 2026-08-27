"""Native Rust + C++ toolchain contract for the public Bazel configs."""

PlatformContractInfo = provider(
    doc = "The native target supported by one Bazel execution platform.",
    fields = {"triple": "Canonical Rust target triple."},
)

def _platform_contract_impl(ctx):
    return [platform_common.ToolchainInfo(
        platform_contract = PlatformContractInfo(triple = ctx.attr.triple),
    )]

platform_contract = rule(
    implementation = _platform_contract_impl,
    attrs = {"triple": attr.string(mandatory = True)},
)

def _foreign_toolchain_rejector_impl(ctx):
    # This target is selectable only when exec != target (see BUILD.bazel).
    # Reject from the selected toolchain implementation itself: an aspect is a
    # useful config-entry diagnostic, but callers can clear or bypass aspects
    # with a direct --platforms flag.  Returning the host C++ provider here
    # would let those callers analyze (and potentially execute) foreign actions
    # with a native compiler masquerading as a cross toolchain.
    fail((
        "unsupported Bazel host/target combination: the selected target " +
        "requires a foreign {kind} toolchain; use the matching native runner"
    ).format(kind = ctx.attr.kind))

foreign_toolchain_rejector = rule(
    implementation = _foreign_toolchain_rejector_impl,
    attrs = {"kind": attr.string(mandatory = True)},
)

def foreign_rejecting_toolchains():
    platforms = {
        "macos_arm64": ["@platforms//os:osx", "@platforms//cpu:aarch64"],
        "linux_x86_64": ["@platforms//os:linux", "@platforms//cpu:x86_64"],
        "linux_arm64": ["@platforms//os:linux", "@platforms//cpu:aarch64"],
        "windows_x86_64": ["@platforms//os:windows", "@platforms//cpu:x86_64"],
        "windows_arm64": ["@platforms//os:windows", "@platforms//cpu:aarch64"],
    }
    # This implementation is selected through the registered toolchains, not
    # built as a top-level product by //... expansion.
    foreign_toolchain_rejector(
        name = "foreign_cc_rejector_impl",
        kind = "C++",
        tags = ["manual"],
    )
    foreign_toolchain_rejector(
        name = "foreign_test_rejector_impl",
        kind = "test execution",
        tags = ["manual"],
    )
    for exec_name, exec_constraints in platforms.items():
        for target_name, target_constraints in platforms.items():
            if exec_name == target_name:
                continue
            native.toolchain(
                name = "foreign_cc_{}_to_{}".format(exec_name, target_name),
                exec_compatible_with = exec_constraints,
                target_compatible_with = target_constraints,
                toolchain = ":foreign_cc_rejector_impl",
                toolchain_type = "@bazel_tools//tools/cpp:toolchain_type",
            )
            native.toolchain(
                name = "foreign_test_{}_to_{}".format(exec_name, target_name),
                exec_compatible_with = exec_constraints,
                target_compatible_with = target_constraints,
                toolchain = ":foreign_test_rejector_impl",
                toolchain_type = "@bazel_tools//tools/test:default_test_toolchain_type",
            )

def _target(ctx):
    public_targets = [
        (ctx.attr._osx, ctx.attr._aarch64, "aarch64-apple-darwin", "macos_arm64"),
        (ctx.attr._linux, ctx.attr._x86_64, "x86_64-unknown-linux-gnu", "linux_x86_64"),
        (ctx.attr._linux, ctx.attr._aarch64, "aarch64-unknown-linux-gnu", "linux_arm64"),
        (ctx.attr._windows, ctx.attr._x86_64, "x86_64-pc-windows-msvc", "windows_x86_64"),
        (ctx.attr._windows, ctx.attr._aarch64, "aarch64-pc-windows-msvc", "windows_arm64"),
    ]
    for os_target, cpu_target, triple, config in public_targets:
        os_constraint = os_target[platform_common.ConstraintValueInfo]
        cpu_constraint = cpu_target[platform_common.ConstraintValueInfo]
        if (ctx.target_platform_has_constraint(os_constraint) and
            ctx.target_platform_has_constraint(cpu_constraint)):
            return triple, config
    return None, None

def _guard(ctx):
    target_triple, target_config = _target(ctx)
    host_triple = ctx.toolchains["//toolchains:platform_contract_type"].platform_contract.triple
    if target_triple == None:
        fail("unsupported Bazel target platform: the public contract only covers macos_arm64, linux_x86_64, linux_arm64, windows_x86_64, and windows_arm64")
    if target_triple != host_triple:
        fail((
            "unsupported Bazel host/target combination: native exec platform {host} " +
            "cannot build --config={config} ({target}); run this config on its matching " +
            "native runner. Cargo target checks are a separate compile-analysis boundary."
        ).format(host = host_triple, config = target_config, target = target_triple))

def _platform_contract_aspect_impl(target, ctx):
    _guard(ctx)
    return []

platform_contract_aspect = aspect(
    implementation = _platform_contract_aspect_impl,
    attrs = {
        "_aarch64": attr.label(default = "@platforms//cpu:aarch64"),
        "_linux": attr.label(default = "@platforms//os:linux"),
        "_osx": attr.label(default = "@platforms//os:osx"),
        "_windows": attr.label(default = "@platforms//os:windows"),
        "_x86_64": attr.label(default = "@platforms//cpu:x86_64"),
    },
    toolchains = ["//toolchains:platform_contract_type"],
)

def _toolchain_probe_impl(ctx):
    _guard(ctx)
    rust = ctx.toolchains["@rules_rust//rust:toolchain_type"]
    cc = ctx.toolchains["@bazel_tools//tools/cpp:toolchain_type"]
    if rust == None:
        fail("native Bazel platform contract is missing its Rust toolchain")
    if cc == None:
        fail("native Bazel platform contract is missing its C++ toolchain")
    output = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(
        output,
        "target={}\nrust={}\ncpp={}\n".format(
            rust.target_triple.str,
            rust.target_triple.str,
            cc.cc.target_gnu_system_name,
        ),
    )
    return [DefaultInfo(files = depset([output]))]

toolchain_probe = rule(
    implementation = _toolchain_probe_impl,
    attrs = {
        "_aarch64": attr.label(default = "@platforms//cpu:aarch64"),
        "_linux": attr.label(default = "@platforms//os:linux"),
        "_osx": attr.label(default = "@platforms//os:osx"),
        "_windows": attr.label(default = "@platforms//os:windows"),
        "_x86_64": attr.label(default = "@platforms//cpu:x86_64"),
    },
    toolchains = [
        "//toolchains:platform_contract_type",
        config_common.toolchain_type("@rules_rust//rust:toolchain_type", mandatory = False),
        config_common.toolchain_type("@bazel_tools//tools/cpp:toolchain_type", mandatory = False),
    ],
)

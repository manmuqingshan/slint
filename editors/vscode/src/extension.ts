// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// This file is the entry point for the vscode extension (not the browser one)

// cSpell: ignore codespace codespaces gnueabihf vsix

import * as path from "node:path";
import { existsSync, mkdirSync, readdirSync, unlinkSync } from "node:fs";
import * as vscode from "vscode";
import { SlintTelemetrySender } from "./telemetry";
import * as common from "./common";
import { NotificationType } from "vscode-languageclient";

import {
    LanguageClient,
    type ServerOptions,
    type ExecutableOptions,
    State,
} from "vscode-languageclient/node";
import { newProject } from "./quick_picks.js";

let statusBar: vscode.StatusBarItem;

const initialTelemetry = {
    preview_opened: 0,
    property_changed: 0,
    data_json_changed: 0,
};

const program_extension = process.platform === "win32" ? ".exe" : "";

interface Platform {
    program_name: string;
    options?: ExecutableOptions;
}

function lsp_panic_log_dir(context: vscode.ExtensionContext) {
    return vscode.Uri.joinPath(context.logUri, "slint-lsp-panics/");
}

function lspPlatform(): Platform | null {
    if (process.platform === "darwin") {
        return {
            program_name: "Slint Live Preview.app/Contents/MacOS/slint-lsp",
        };
    }
    if (process.platform === "linux") {
        let remote_env_options = null;
        if (typeof vscode.env.remoteName !== "undefined") {
            remote_env_options = {
                DISPLAY: ":0",
            };
        }
        if (process.arch === "x64") {
            return {
                program_name: "slint-lsp-x86_64-unknown-linux-gnu",
                options: {
                    env: remote_env_options,
                },
            };
        }
        if (process.arch === "arm") {
            return {
                program_name: "slint-lsp-armv7-unknown-linux-gnueabihf",
                options: {
                    env: remote_env_options,
                },
            };
        }
        if (process.arch === "arm64") {
            return {
                program_name: "slint-lsp-aarch64-unknown-linux-gnu",
                options: {
                    env: remote_env_options,
                },
            };
        }
    } else if (process.platform === "win32") {
        if (process.arch === "arm64") {
            return {
                program_name: "slint-lsp-aarch64-pc-windows-msvc.exe",
            };
        }
        return {
            program_name: "slint-lsp-x86_64-pc-windows-msvc.exe",
        };
    }
    return null;
}

function find_lsp_binary(
    context: vscode.ExtensionContext,
    lsp_platform: Platform,
) {
    const config = vscode.workspace.getConfiguration("slint");
    const lsp_binary_path = config.lspBinaryPath;

    if (lsp_binary_path === "") {
        // No slint-lsp override configured: Try a local ../target build first, then try the plain bundled binary and
        // finally the architecture specific one. A debug session will find the first one, a local package build the
        // second and the distributed vsix the last.
        const lspSearchPaths = [
            path.join(
                context.extensionPath,
                "..",
                "..",
                "target",
                "debug",
                "slint-lsp" + program_extension,
            ),
            path.join(
                context.extensionPath,
                "..",
                "..",
                "target",
                "release",
                "slint-lsp" + program_extension,
            ),
            path.join(
                context.extensionPath,
                "bin",
                "slint-lsp" + program_extension,
            ),
            path.join(context.extensionPath, "bin", lsp_platform.program_name),
        ];

        const serverModule = lspSearchPaths.find((path) => existsSync(path));

        if (serverModule === undefined) {
            console.warn(
                "Could not locate slint-lsp server binary, neither in bundled bin/ directory nor relative in ../target",
            );
        }
        return serverModule;
    }

    if (path.isAbsolute(lsp_binary_path)) {
        // The slint-lsp override is a absolute path: Look in that path only
        if (existsSync(lsp_binary_path)) {
            return lsp_binary_path;
        }
        console.warn(
            "Could not locate the configured slint-lsp server binary at absolute path",
        );
        return undefined;
    }

    // The slint-lsp override is a relative path: Look relative to PATH
    const path_env = process.env.PATH?.split(path.delimiter).map((p) =>
        path.join(p, lsp_binary_path),
    );

    const serverModule = path_env?.find((path) => existsSync(path));
    if (serverModule === undefined) {
        console.warn(
            "Could not locate the configured slint-lsp server binary in PATH",
        );
    }
    return serverModule;
}

// Please add changes to the BaseLanguageClient via
// `client.add_updater((cl: BaseLanguageClient | null): void)`
//
// That makes sure the code is run even when the LSP gets restarted, etc.
//
// Please add setup common between web and native VSCode by adding updaters
// to the client in common.ts!
function startClient(
    client: common.ClientHandle,
    context: vscode.ExtensionContext,
    telemetryLogger: vscode.TelemetryLogger,
) {
    const lsp_platform = lspPlatform();
    if (lsp_platform === null) {
        return;
    }

    const serverModule = find_lsp_binary(context, lsp_platform);

    if (serverModule === undefined) {
        return;
    }

    const options = Object.assign({}, lsp_platform.options);
    options.env = Object.assign({}, process.env, lsp_platform.options?.env);

    const devBuild = serverModule.includes("/target/debug/");
    if (devBuild) {
        options.env["RUST_BACKTRACE"] = "1";
    }

    options.env["SLINT_LSP_PANIC_LOG_DIR"] = lsp_panic_log_dir(context).fsPath;

    const args = vscode.workspace
        .getConfiguration("slint")
        .get<[string]>("lsp-args");

    const serverOptions: ServerOptions = {
        run: { command: serverModule, options: options, args: args },
        debug: { command: serverModule, options: options, args: args },
    };

    // Add setup common between native and wasm LSP to common.setup_client_handle!
    client.add_updater((cl) => {
        // Just make sure that the output channel is always present.
        cl?.outputChannel.append("");

        cl?.onDidChangeState((event) => {
            const properly_stopped = cl.hasOwnProperty("slint_stopped");
            if (
                !properly_stopped &&
                event.newState === State.Stopped &&
                event.oldState === State.Running
            ) {
                cl.outputChannel.appendLine(
                    "The Slint Language Server crashed. This is a bug, Please open an issue on the [Slint bug tracker](https://github.com/slint-ui/slint/issues).",
                );
                cl.outputChannel.show();
                vscode.window.showErrorMessage(
                    "The Slint Language Server crashed. Please open a bug on the [Slint bug tracker](https://github.com/slint-ui/slint/issues) with the panic message.",
                );
            }
        });

        cl?.onNotification(
            new NotificationType("telemetry/event"),
            (params: any) => {
                handleTelemetryEvent(params.type, context.globalState);
            },
        );
    });

    const cl = new LanguageClient(
        "slint-lsp",
        "Slint",
        serverOptions,
        common.languageClientOptions(["file"], telemetryLogger),
    );

    common.prepare_client(cl);

    cl.start().then(() => (client.client = cl));
}

export function activate(context: vscode.ExtensionContext) {
    // Disable native preview in Codespace.
    //
    // We want to have a good default (WASM preview), but we also need to
    // support users that have special setup in place that allows them to run
    // the native previewer remotely.
    if (process.env.hasOwnProperty("CODESPACES")) {
        vscode.workspace
            .getConfiguration("slint")
            .update(
                "preview.providedByEditor",
                true,
                vscode.ConfigurationTarget.Global,
            );
    }

    const telemetryLogger = vscode.env.createTelemetryLogger(
        new SlintTelemetrySender(context.extensionMode),
        {
            ignoreBuiltInCommonProperties: true,
            additionalCommonProperties: {
                common: {
                    machineId: vscode.env.machineId,
                    extname: context.extension.packageJSON.name,
                    extversion: context.extension.packageJSON.version,
                    vscodeversion: vscode.version,
                    platform: process.platform,
                    language: vscode.env.language,
                },
            },
        },
    );

    statusBar = common.activate(context, (cl, ctx) =>
        startClient(cl, ctx, telemetryLogger),
    );

    context.subscriptions.push(
        vscode.commands.registerCommand("slint.newProject", newProject),
    );

    startTelemetryTimer(context, telemetryLogger);

    // Create a file system watcher to watch for panic logs:
    const panic_dir = lsp_panic_log_dir(context);
    const ensureDirectoryExists = (dir: string) => {
        if (!existsSync(dir)) {
            mkdirSync(dir, { recursive: true });
        } else {
            readdirSync(dir).forEach((file) => {
                unlinkSync(path.join(dir, file));
            });
        }
    };

    ensureDirectoryExists(panic_dir.fsPath);
    const watcher = vscode.workspace.createFileSystemWatcher(
        new vscode.RelativePattern(panic_dir, "slint_lsp_panic_*.log"),
    );

    const lsp_platform = lspPlatform();
    if (lsp_platform === null) {
        return;
    }

    const serverModule = find_lsp_binary(context, lsp_platform);

    if (serverModule === undefined) {
        return;
    }

    const custom_lsp = !serverModule.startsWith(
        path.join(context.extensionPath, "bin"),
    );

    const version_extension = custom_lsp ? " [CUSTOM BINARY]" : "";

    watcher.onDidCreate((uri) => {
        vscode.workspace.fs.readFile(uri).then((data) => {
            const contents = Buffer.from(data).toString("utf-8");
            const lines = contents.split("\n");
            const version = lines[0] + version_extension;
            // Location is trusted because it is a path within the LSP (as build on our CI)
            const location = new vscode.TelemetryTrustedValue(lines[1]);
            const backtrace = lines[2];
            const message = lines.slice(3).join("\n");
            telemetryLogger.logError("lsp-panic", {
                version: version,
                location: location,
                message: message,
                backtrace: backtrace,
            });
            console.log("Removing file");
            vscode.workspace.fs.delete(uri);
        });
    });

    // Ensure the watcher is disposed of when the extension is deactivated
    context.subscriptions.push(watcher);
}

export function deactivate(): Thenable<void> | undefined {
    return common.deactivate();
}

function handleTelemetryEvent(
    telemetryType: string,
    globalState: vscode.Memento,
) {
    const telemetryState: Record<string, number> = globalState.get(
        "telemetryState",
        JSON.parse(JSON.stringify(initialTelemetry)),
    );
    telemetryState[telemetryType] += 1;
    globalState.update("telemetryState", telemetryState);
}

function startTelemetryTimer(
    context: vscode.ExtensionContext,
    telemetryLogger: vscode.TelemetryLogger,
) {
    checkForTelemetry();

    const _oneHourTimer = setInterval(
        () => {
            checkForTelemetry();
        },
        1000 * 60 * 60,
    );

    function checkForTelemetry() {
        const now = Date.now();
        const timeSinceLastUsage =
            now - context.globalState.get("lastUsage", 0);

        if (timeSinceLastUsage > 1000 * 60 * 59) {
            context.globalState.update("lastUsage", now);

            const telemetryState = context.globalState.get(
                "telemetryState",
                {},
            );

            const hasPositiveValue = Object.values(
                telemetryState as Record<string, number>,
            ).some((value) => value > 0);

            if (hasPositiveValue) {
                telemetryLogger.logUsage("live-preview-stats", telemetryState);

                context.globalState.update(
                    "telemetryState",
                    JSON.parse(JSON.stringify(initialTelemetry)),
                );
            }
        }
    }
}

const $fs = require("fs");
const $glob = require("glob");
const $path = require("path");
const $child = require("child_process");
const $toml = require("toml");
const $rimraf = require("rimraf");
const { createFilter } = require("rollup-pluginutils");

function posixPath(path) {
    return path.replace(/\\/g, $path.posix.sep);
}

function glob(pattern, cwd) {
    return new Promise(function (resolve, reject) {
        $glob(pattern, {
            cwd: cwd,
            strict: true,
            absolute: true,
            nodir: true
        }, function (err, files) {
            if (err) {
                reject(err);

            } else {
                resolve(files);
            }
        });
    });
}

function rm(path) {
    return new Promise(function (resolve, reject) {
        $rimraf(path, { glob: false }, function (err) {
            if (err) {
                reject(err);

            } else {
                resolve();
            }
        });
    });
}

function read(path) {
    return new Promise(function (resolve, reject) {
        $fs.readFile(path, function (err, file) {
            if (err) {
                reject(err);

            } else {
                resolve(file);
            }
        });
    });
}

function write(path, content) {
    return new Promise(function (resolve, reject) {
        $fs.writeFile(path, content, 'utf-8', function (err, file) {
            if (err) {
                reject(err);

            } else {
                resolve(file);
            }
        });
    });
}

function wait(p) {
    return new Promise((resolve, reject) => {
        p.on("close", (code) => {
            if (code === 0) {
                resolve();

            } else {
                reject(new Error("Command `" + p.spawnargs.join(" ") + "` failed with error code: " + code));
            }
        });

        p.on("error", reject);
    });
}

const state = {
    locked: false,
    pending: [],
};

async function lock(f) {
    if (state.locked) {
        await new Promise(function (resolve, reject) {
            state.pending.push(resolve);
        });

        if (state.locked) {
            throw new Error("Invalid lock state");
        }
    }

    state.locked = true;

    try {
        return await f();

    } finally {
        state.locked = false;

        if (state.pending.length !== 0) {
            const resolve = state.pending.shift();
            // Wake up pending task
            resolve();
        }
    }
}

async function get_target_dir(dir) {
    return "target";
}

async function wasm_pack(cx, state, dir, source, id, options) {
    const target_dir = await get_target_dir(dir);

    const toml = $toml.parse(source);

    const name = toml.package.name;

    const out_dir = $path.resolve($path.join(target_dir, "wasm-pack", name));

    await rm(out_dir);

    // TODO: customize the name
    const args = [
        "--log-level", (options.verbose ? "info" : "error"),
        "build",
        "--out-dir", out_dir,
        "--out-name", "index",
        "--target", "bundler",
        (options.debug ? "--dev" : "--release"),
        "--",
    ].concat(options.cargoArgs);

    const command = (process.platform === "win32" ? "wasm-pack.cmd" : "wasm-pack");

    try {
        await lock(async function () {
            await wait($child.spawn(command, args, { cwd: dir, stdio: "inherit" }));
        });

    } catch (e) {
        if (e.code === "ENOENT") {
            throw new Error("Could not find wasm-pack, install it with `yarn add --dev wasm-pack` or `npm install --save-dev wasm-pack`");

        } else if (options.verbose) {
            throw e;

        } else {
            throw new Error("Rust compilation failed");
        }
    }

    // TODO: customize the name
    const import_path = JSON.stringify("./" + posixPath($path.relative(dir, $path.join(out_dir, "index.js"))));
    const WASM_FILE_NAME = "index_bg";
    const binaryPath = $path.join(out_dir, WASM_FILE_NAME+'.wasm');
    const wasm = await read(binaryPath);

    const separate_base64_wasm = wasmToBase64(wasm);

    // copy wasm for use in node and webpack projects
    const target = binaryPath;
    console.log(target);
    const dest = $path.join(dir, 'dist', WASM_FILE_NAME+'.wasm');
    console.log('dest', dest)
    $fs.copyFileSync(target, dest)

    // generate base64 encoded version of the file
    cx.emitFile({
        type: "asset",
        source: separate_base64_wasm,
        fileName: 'wasm.base64.js'
    });

    const wasmPath = posixPath($path.relative(dir, $path.join(out_dir, "wasm.base64.js")));
    await write(wasmPath, separate_base64_wasm, 'utf8');

    return {
        code: `
            import * as exports from ${import_path};

            export function loadFile(url) {
                return new Promise((resolve, reject) => {
                    require("fs").readFile(url, (err, data) => {
                        if (err) {
                            reject(err);
                        } else {
                            resolve(data);
                        }
                    });
                });
            }

            export async function loadWASM() {
                let isNode = typeof process !== 'undefined' && process.versions != null && process.versions.node != null
                if (isNode) {
                    const path = require('path').join(__dirname, '${WASM_FILE_NAME}.wasm');
                    const bytes = await loadFile(path);
                    let imports = {};
                    imports['./${WASM_FILE_NAME}.js'] = exports;
                    const module = await WebAssembly.compile(bytes);
                    const instance = await WebAssembly.instantiate(module, imports);

                    return instance.exports;
                }

                // TODO: if CJS in the browser load via script ???
                // import('@solana/wasm/dist/solana.wasm').then(xyz => xyz());

                // TODO: if ESM use dynamic import (add instruction for webpack to copy file)

                // TODO: if IIF load script base64 ???? (import.meta.url?)
            }

            export async function init (path) {
                let instance = await loadWASM();
                Object.assign(global.__wasm, instance);
                exports.setup();
                return exports;
            };
        `,
        map: { mappings: {} }
    };
}

function wasmToBase64(wasm) {
    const base64_decode = `
        const base64codes = [62,0,0,0,63,52,53,54,55,56,57,58,59,60,61,0,0,0,0,0,0,0,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,0,0,0,0,0,0,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51];
        function getBase64Code(charCode) {
            return base64codes[charCode - 43];
        }
        function base64_decode(str) {
            let missingOctets = str.endsWith("==") ? 2 : str.endsWith("=") ? 1 : 0;
            let n = str.length;
            let result = new Uint8Array(3 * (n / 4));
            let buffer;
            for (let i = 0, j = 0; i < n; i += 4, j += 3) {
                buffer =
                    getBase64Code(str.charCodeAt(i)) << 18 |
                    getBase64Code(str.charCodeAt(i + 1)) << 12 |
                    getBase64Code(str.charCodeAt(i + 2)) << 6 |
                    getBase64Code(str.charCodeAt(i + 3));
                result[j] = buffer >> 16;
                result[j + 1] = (buffer >> 8) & 0xFF;
                result[j + 2] = buffer & 0xFF;
            }
            return result.subarray(0, result.length - missingOctets);
        }
    `;

    const wasm_string = JSON.stringify(wasm.toString("base64"));

    const separate_base64_wasm = `
        ${base64_decode}
        function __loadWASM() {
            return base64_decode(${wasm_string});
        };
    `;
    return separate_base64_wasm;
}

async function watch_files(cx, dir, options) {
    if (options.watch) {
        const matches = await Promise.all(options.watchPatterns.map(function (pattern) {
            return glob(pattern, dir);
        }));

        // TODO deduplicate matches ?
        matches.forEach(function (files) {
            files.forEach(function (file) {
                cx.addWatchFile(file);
            });
        });
    }
}

async function build(cx, state, source, id, options) {
    const dir = $path.dirname(id);

    const [output] = await Promise.all([
        wasm_pack(cx, state, dir, source, id, options),
        watch_files(cx, dir, options),
    ]);

    return output;
}


export const rust = (options = {}) => {
    // TODO should the filter affect the watching ?
    // TODO should the filter affect the Rust compilation ?
    const filter = createFilter(options.include, options.exclude);

    const state = {};

    if (options.watchPatterns == null) {
        options.watchPatterns = [
            "src/**"
        ];
    }

    if (options.importHook == null) {
        options.importHook = function (path) { return JSON.stringify(path); };
    }

    if (options.serverPath == null) {
        options.serverPath = "";
    }

    if (options.cargoArgs == null) {
        options.cargoArgs = [];
    }

    if (options.verbose == null) {
        options.verbose = false;
    }

    return {
        name: "rust",

        buildStart(rollup) {
            if (this.meta.watchMode || rollup.watch) {
                if (options.watch == null) {
                    options.watch = true;
                }

                if (options.debug == null) {
                    options.debug = true;
                }
            }
        },

        transform(source, id) {
            // ignore renference to wasm since they are handled differently
            if ($path.basename(id).endsWith('wasm') && filter(id)) {
                const dir = $path.dirname(id);
                const wasmPath = posixPath($path.relative(dir, "wasm.base64.js"));
            
                
                
                return {
                    code: `
                        global.__wasm = {};
                        export const wasm = __wasm;

                        export default wasm;
                    `,
                    syntheticNamedExports: ['wasm'],
                    map: { mappings: '' }
                };
            }

            if ($path.basename(id) === "Cargo.toml" && filter(id)) {
                return build(this, state, source, id, options);

            } else {
                return null;
            }
        },

        resolveFileUrl(info) {
            if (info.referenceId === state.fileId) {
                return options.importHook(options.serverPath + info.fileName);

            } else {
                return null;
            }
        },
    };
};

export default rust;
/******/ (() => { // webpackBootstrap
/******/ 	"use strict";
/******/ 	/* webpack/runtime/compat */
/******/ 	
/******/ 	if (typeof __nccwpck_require__ !== 'undefined') __nccwpck_require__.ab = __dirname + "/";
/******/ 	
/************************************************************************/
var __webpack_exports__ = {};
// This entry need to be wrapped in an IIFE because it uses a non-standard name for the exports (exports).
(() => {
var exports = __webpack_exports__;

Object.defineProperty(exports, "__esModule", ({ value: true }));
exports.SCOPED_ENV = void 0;
exports.childEnv = childEnv;
exports.splitCommand = splitCommand;
/** Names that must exist only in the spawned aster process. */
exports.SCOPED_ENV = [
    "ACTIONS_RESULTS_URL",
    "ACTIONS_RUNTIME_TOKEN",
    "ASTER_REMOTE_CACHE",
    "ASTER_CACHE_MODE",
];
/** Child env: the parent env plus the cache token, never exported upward. */
function childEnv(request) {
    const env = {};
    for (const [key, value] of Object.entries(request.parentEnv)) {
        if (value !== undefined) {
            env[key] = value;
        }
    }
    env.ACTIONS_RESULTS_URL = request.resultsUrl;
    env.ACTIONS_RUNTIME_TOKEN = request.runtimeToken;
    env.ASTER_REMOTE_CACHE = "gha";
    env.ASTER_CACHE_MODE = request.cacheMode;
    return env;
}
/**
 * Split `run` into aster arguments.
 * Single quotes are literal. Double quotes honor backslash escapes.
 * A leading `aster` token is the program name, which this action already supplies.
 */
function splitCommand(command) {
    const parts = splitQuoted(command);
    if (parts.length === 0) {
        throw new Error("run input is empty");
    }
    if (parts[0] === "aster") {
        throw new Error("run takes aster arguments, not the program name");
    }
    return parts;
}
function splitQuoted(command) {
    const parts = [];
    let current = "";
    let inWord = false;
    let quote = null;
    for (let i = 0; i < command.length; i++) {
        const ch = command[i];
        if (quote === "'") {
            if (ch === "'") {
                quote = null;
            }
            else {
                current += ch;
            }
            continue;
        }
        if (quote === '"') {
            if (ch === '"') {
                quote = null;
                continue;
            }
            if (ch === "\\") {
                const next = command[i + 1];
                if (next === undefined) {
                    throw new Error("run input has a trailing backslash");
                }
                if (next === "$" || next === "`" || next === '"' || next === "\\" || next === "\n") {
                    if (next !== "\n") {
                        current += next;
                    }
                    i++;
                    continue;
                }
            }
            current += ch;
            continue;
        }
        if (ch === "'" || ch === '"') {
            quote = ch;
            inWord = true;
            continue;
        }
        if (ch === "\\") {
            const next = command[i + 1];
            if (next === undefined) {
                throw new Error("run input has a trailing backslash");
            }
            if (next !== "\n") {
                current += next;
                inWord = true;
            }
            i++;
            continue;
        }
        if (isSpace(ch)) {
            if (inWord) {
                parts.push(current);
                current = "";
                inWord = false;
            }
            continue;
        }
        current += ch;
        inWord = true;
    }
    if (quote !== null) {
        throw new Error("run input has an unbalanced quote");
    }
    if (inWord) {
        parts.push(current);
    }
    return parts;
}
function isSpace(ch) {
    return ch === " " || ch === "\t" || ch === "\n" || ch === "\r" || ch === "\v" || ch === "\f";
}

})();

module.exports = __webpack_exports__;
/******/ })()
;
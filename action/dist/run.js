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
/** Split a shell-less command. Quotes are not interpreted. */
function splitCommand(command) {
    const parts = command.trim().split(/\s+/);
    if (parts.length === 0 || parts[0] === "") {
        throw new Error("run input is empty");
    }
    return parts;
}

})();

module.exports = __webpack_exports__;
/******/ })()
;
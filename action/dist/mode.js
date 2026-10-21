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
exports.selectCacheMode = selectCacheMode;
const OVERRIDES = new Set(["rw", "read", "off"]);
/**
 * `auto` writes on push, schedule, and workflow_dispatch to the default
 * branch. Every other event, including a missing payload, is read-only.
 * `rw`, `read`, and `off` override that.
 */
function selectCacheMode(input) {
    const requested = input.cacheMode.trim();
    if (OVERRIDES.has(requested)) {
        return requested;
    }
    if (requested !== "auto" && requested !== "") {
        throw new Error(`invalid cache-mode ${JSON.stringify(requested)}; expected auto, rw, read, or off`);
    }
    const writable = (input.eventName === "push" ||
        input.eventName === "schedule" ||
        input.eventName === "workflow_dispatch") &&
        input.defaultBranch !== "" &&
        input.ref === `refs/heads/${input.defaultBranch}`;
    return writable ? "rw" : "read";
}

})();

module.exports = __webpack_exports__;
/******/ })()
;
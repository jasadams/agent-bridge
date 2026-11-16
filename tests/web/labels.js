"use strict";
// AGB-60: the page labels agents only with names the broker resolved. Run by tests/web_labels.rs with Node.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const window = {};
new Function("window", fs.readFileSync(path.join(__dirname, "../../assets/web/util.js"), "utf8"))(window);
const { endpointLabelName, postAuthorLabel } = window.AB;
const id = "claude-97e8d81e-073d-4cde-a017-4a8255b7b3b8";
const ref = { id, name: "Claude b3b8", provider: "claude", machine: "laptop" };

// A resolved agent shows its name, never its ID.
assert.equal(endpointLabelName({ agent: id, ref }), "Claude b3b8");
// With no reference there is no ID-derived label either.
assert.equal(endpointLabelName({ agent: id }), "Unknown agent");
assert.equal(endpointLabelName({ agent: "user" }), "You");
assert.equal(endpointLabelName(undefined), "Unknown agent");
for (const label of [endpointLabelName({ agent: id }), endpointLabelName({ agent: id, ref })]) assert.ok(!label.includes(id));

const post = { author: { agent_id: `inbox-${id}` }, author_display_name: `inbox-${id}`, author_name: "Claude b3b8" };
assert.equal(postAuthorLabel(post), "Claude b3b8");
assert.equal(postAuthorLabel({ ...post, author_name: undefined }), "Unknown agent");
assert.equal(postAuthorLabel({ author: { agent_id: "inbox-user" }, author_name: "x" }), "You");
console.log("ok");

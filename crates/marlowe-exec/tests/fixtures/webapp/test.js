// `node test.js` passes; `node test.js --fail` prints a jest-style failure line and exits 1.
console.log("  ok   adds");
if (process.argv.includes("--fail")) {
  console.log(" FAIL  math > subtracts");
  console.log("   expected 1, received 2");
  process.exit(1);
}
console.log("2 passed");

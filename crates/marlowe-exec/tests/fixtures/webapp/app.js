// The planted problems: a console error at load, and an uncaught exception a moment later.
console.error("planted console error: widget failed to initialise");
setTimeout(function () {
  undefinedWidget.render();
}, 30);

document.getElementById("load").addEventListener("click", async function () {
  const response = await fetch("/api/items");
  const data = await response.json();
  document.getElementById("items").innerHTML = data.items.map(function (i) { return "<li>" + i.name + "</li>"; }).join("");
  document.getElementById("status").textContent = "Loaded " + data.items.length + " items";
});

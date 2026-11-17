// Scroll positions on the dashboard (issue 748). A live refresh replaces
// a section's contents, which would put every scrollable region in it,
// the section's own scroll included, back at the start. Each region's
// place is kept before the swap and given back after it, a region
// scrolled to its end stays at the end as it grows, as a terminal does,
// and a section the reader is scrolling at that moment is left for the
// next refresh. The page itself keeps its place too.

(function () {
  "use strict";

  // How long after the reader last scrolled a region its section is left
  // alone, and how near its end a region counts as at the end, in pixels.

  var QUIET_MS = 1000;
  var END_SLACK = 4;

  var lastScrolled = null;
  var lastScrolledAt = 0;
  var kept = new WeakMap();

  function scrolls(element) {
    return element.scrollHeight > element.clientHeight + 1 ||
      element.scrollWidth > element.clientWidth + 1;
  }

  // A region is named by its id, or by the path to it from the section:
  // each step its tag and its place among the siblings of that tag, which
  // a swap that redraws the same structure keeps.

  function nameOf(element, root) {
    if (element === root) {
      return "";
    }

    if (element.id) {
      return "#" + element.id;
    }

    var steps = [];

    for (var node = element; node && node !== root; node = node.parentElement) {
      var place = 0;

      for (var sibling = node.previousElementSibling; sibling; sibling = sibling.previousElementSibling) {
        if (sibling.tagName === node.tagName) {
          place += 1;
        }
      }

      steps.unshift(node.tagName + ":" + place);
    }

    return steps.join("/");
  }

  function find(name, root) {
    if (name === "") {
      return root;
    }

    if (name.charAt(0) === "#") {
      var found = document.getElementById(name.slice(1));
      return found && root.contains(found) ? found : null;
    }

    var node = root;

    name.split("/").forEach(function (step) {
      if (!node) {
        return;
      }

      var parts = step.split(":");
      var place = Number(parts[1]);
      var match = null;

      for (var child = node.firstElementChild; child; child = child.nextElementSibling) {
        if (child.tagName === parts[0] && place-- === 0) {
          match = child;
          break;
        }
      }

      node = match;
    });

    return node;
  }

  document.addEventListener("scroll", function (event) {
    if (event.target instanceof Element) {
      lastScrolled = event.target;
      lastScrolledAt = Date.now();
    }
  }, true);

  document.addEventListener("htmx:beforeSwap", function (event) {
    var root = event.detail.target;

    if (!root) {
      return;
    }

    // A region the reader is scrolling now keeps its section as it is.

    if (lastScrolled && root.contains(lastScrolled) && Date.now() - lastScrolledAt < QUIET_MS) {
      event.detail.shouldSwap = false;
      return;
    }

    var regions = [];

    [root].concat(Array.prototype.slice.call(root.querySelectorAll("*"))).forEach(function (element) {
      if (!scrolls(element) || (element.scrollTop === 0 && element.scrollLeft === 0)) {
        return;
      }

      regions.push({
        name: nameOf(element, root),
        top: element.scrollTop,
        left: element.scrollLeft,
        atEnd: element.scrollTop > 0 &&
          element.scrollTop + element.clientHeight >= element.scrollHeight - END_SLACK,
      });
    });

    kept.set(root, { regions: regions, x: window.scrollX, y: window.scrollY });
  });

  document.addEventListener("htmx:afterSwap", function (event) {
    var root = event.detail.target;
    var saved = root && kept.get(root);

    if (!saved) {
      return;
    }

    kept.delete(root);

    saved.regions.forEach(function (region) {
      var element = find(region.name, root);

      if (!element) {
        return;
      }

      element.scrollLeft = region.left;
      element.scrollTop = region.atEnd ? element.scrollHeight : region.top;
    });

    if (window.scrollX !== saved.x || window.scrollY !== saved.y) {
      window.scrollTo(saved.x, saved.y);
    }
  });
})();

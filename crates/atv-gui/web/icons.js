// The icon list from the approved design (not published): one outline set on
// a 16 x 16 grid, 1.3 to 1.4 px stroke, round caps and joins, in currentColor.
// Each entry is the inner SVG markup.

export const ICONS = {
  video: '<rect x="1.8" y="3.3" width="12.4" height="9.4" rx="2.2"/><path d="M6.7 6.1v3.8l3.1-1.9z" fill="currentColor" stroke="none"/>',
  "video-outline": '<rect x="1.8" y="3.3" width="12.4" height="9.4" rx="2.2"/><path d="M6.7 6.1v3.8l3.1-1.9z"/>',
  document: '<path d="M4 1.8h5l3.2 3.2v9.2H4z"/><path d="M9 1.8V5h3.2"/>',
  clock: '<circle cx="8" cy="8" r="6"/><path d="M8 4.8v3.4l2.2 1.4"/>',
  sliders: '<path d="M2.2 5h2M8 5h5.8M2.2 11h5.8M12 11h1.8"/><circle cx="6" cy="5" r="1.8"/><circle cx="10" cy="11" r="1.8"/>',
  folder: '<path d="M1.8 4.3c0-.5.4-.9.9-.9h3.2l1.5 1.5h5.9c.5 0 .9.4.9.9v6.4c0 .5-.4.9-.9.9H2.7c-.5 0-.9-.4-.9-.9z"/>',
  archive: '<rect x="1.8" y="2.8" width="12.4" height="3.2" rx="0.9"/><path d="M2.8 6v6.4c0 .5.4.9.9.9h8.6c.5 0 .9-.4.9-.9V6M6.4 8.6h3.2"/>',
  "tray-in": '<path d="M8 2v7.2M5.2 6.6L8 9.4l2.8-2.8"/><path d="M1.8 9.6l1.6-3.4h1.4M11.2 6.2h1.4l1.6 3.4v3.2c0 .5-.4.9-.9.9H2.7c-.5 0-.9-.4-.9-.9z"/><path d="M1.8 9.6h3.4l.8 1.4h4l.8-1.4h3.4"/>',
  plus: '<path d="M8 3.5v9M3.5 8h9"/>',
  close: '<path d="M4 4l8 8M12 4l-8 8"/>',
  eye: '<path d="M1.5 8s2.4-4.4 6.5-4.4S14.5 8 14.5 8s-2.4 4.4-6.5 4.4S1.5 8 1.5 8z"/><circle cx="8" cy="8" r="2"/>',
  "eye-off": '<path d="M1.5 8s2.4-4.4 6.5-4.4S14.5 8 14.5 8s-2.4 4.4-6.5 4.4S1.5 8 1.5 8z"/><circle cx="8" cy="8" r="2"/><path d="M2.5 2.5l11 11"/>',
  key: '<circle cx="5" cy="8" r="2.7"/><path d="M7.7 8h6.3M12 8v2.2M10 8v1.6"/>',
  lock: '<rect x="3" y="7" width="10" height="7.2" rx="1.6"/><path d="M5.2 7V5a2.8 2.8 0 015.6 0v2"/>',
  info: '<circle cx="8" cy="8" r="6.2"/><path d="M8 7.3v3.6"/><circle cx="8" cy="5.1" r="0.5" fill="currentColor"/>',
  "error-circle": '<circle cx="8" cy="8" r="6.2"/><path d="M8 4.8v3.6"/><circle cx="8" cy="10.9" r="0.5" fill="currentColor"/>',
  warning: '<path d="M8 2.2l6.2 11H1.8z"/><path d="M8 6.6v3"/><circle cx="8" cy="11.4" r="0.5" fill="currentColor"/>',
  check: '<path d="M3.5 8.4l3 3 6-6.6"/>',
  "seal-check": '<path d="M8 1.6l1.6 1.2 2-.1.6 1.9 1.6 1.2-.6 1.9.6 1.9-1.6 1.2-.6 1.9-2-.1L8 14.4l-1.6-1.2-2 .1-.6-1.9-1.6-1.2.6-1.9-.6-1.9 1.6-1.2.6-1.9 2 .1z"/><path d="M5.6 8.1l1.6 1.6 3.2-3.3"/>',
  "shield-check": '<path d="M8 1.8l5.2 2v4c0 3.2-2.2 5.4-5.2 6.4-3-1-5.2-3.2-5.2-6.4v-4z"/><path d="M5.7 8.1l1.6 1.6 3-3.1"/>',
  "folder-reveal": '<path d="M1.8 4.3c0-.5.4-.9.9-.9h3.2l1.5 1.5h5.9c.5 0 .9.4.9.9v6.4c0 .5-.4.9-.9.9H2.7c-.5 0-.9-.4-.9-.9z"/><path d="M8 7v4M6.2 9.2L8 11l1.8-1.8"/>',
  play: '<path d="M5.4 3.4v9.2L12.8 8z" fill="currentColor"/>',
  link: '<path d="M6.6 9.4l2.8-2.8"/><path d="M7.4 4.8l1.1-1.1a2.6 2.6 0 013.7 3.7l-1.1 1.1M8.6 11.2l-1.1 1.1a2.6 2.6 0 01-3.7-3.7l1.1-1.1"/>',
  globe: '<circle cx="8" cy="8" r="6.2"/><path d="M1.8 8h12.4M8 1.8c1.7 1.8 2.5 3.8 2.5 6.2S9.7 12.4 8 14.2C6.3 12.4 5.5 10.4 5.5 8S6.3 3.6 8 1.8z"/>',
  "video-off": '<rect x="1.8" y="3.3" width="12.4" height="9.4" rx="2.2"/><path d="M6 6l4 4M10 6l-4 4"/>',
};

/** An SVG element string for an icon. */
export function icon(name, size = 16, stroke = 1.4) {
  const body = ICONS[name] || "";
  return `<svg width="${size}" height="${size}" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="${stroke}" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;
}

/** Replace every `<span data-icon="name" data-size="16" data-stroke="1.4">` with its SVG. */
export function fillIcons(root = document) {
  root.querySelectorAll("[data-icon]").forEach((el) => {
    el.outerHTML = icon(el.dataset.icon, Number(el.dataset.size || 16), Number(el.dataset.stroke || 1.4));
  });
}

const visible = (node) => !!node && node.getClientRects().length > 0 && getComputedStyle(node).visibility !== 'hidden';
const text = (selector) => [...document.querySelectorAll(selector)]
    .filter(visible).map((node) => node.innerText || '').join('\n').trim();
const table = document.querySelector('.rc-imageselect-table-33, .rc-imageselect-table-44');
const cells = table ? [...table.querySelectorAll('td')] : [];
const rows = table ? table.querySelectorAll('tr').length : 0;
const cols = rows ? cells.length / rows : 0;
const rectangle = table?.getBoundingClientRect();
const geometry = rectangle ? [rectangle.x, rectangle.y, rectangle.width, rectangle.height]
    .map((value) => Math.round(value * 10) / 10) : [];
const active = (animations) => animations.some((animation) => animation.pending || animation.playState === 'running');
let moving = !!table && active(table.getAnimations?.({subtree:true}) || []);
for (let parent = table?.parentElement; parent; parent = parent.parentElement) {
    moving ||= active(parent.getAnimations?.() || []);
}
const prompt = text('.rc-imageselect-desc-wrapper');
const opaque = (node, stop) => {
    for (let current = node; current; current = current.parentElement) {
        if (Number(getComputedStyle(current).opacity) < 0.99) return false;
        if (current === stop) break;
    }
    return true;
};
const loading = cells.some((cell) => cell.matches('.rc-imageselect-dynamic-selected')
    || !!cell.querySelector('.rc-imageselect-dynamic-selected')
    || [...cell.querySelectorAll('.rc-image-tile-overlay')].some((overlay) =>
        visible(overlay) && Number(getComputedStyle(overlay).opacity) > 0.01));
const imagesLoaded = cells.length > 0 && cells.every((cell) => {
    const images = [...cell.querySelectorAll('img')];
    return images.length > 0 && images.every((image) => image.complete && image.naturalWidth > 0 && opaque(image, cell));
});
const blocked = !!text('.rc-doscaptcha-header, .rc-doscaptcha-body');
const unsupported = !blocked && !table && !!text('.rc-audiochallenge-instructions, .rc-textchallenge-instructions');
return {
    prompt, rows, cols,
    dynamic: /once there are none left|until there are none|new images|images fade/i.test(prompt),
    ready: visible(table) && !!prompt && imagesLoaded && !loading && !moving && (cells.length === 9 || cells.length === 16),
    geometry, moving,
    identities: cells.map((cell) => JSON.stringify([...cell.querySelectorAll('img')].map((image) => [
        image.currentSrc || image.src, image.getAttribute('style') || '', image.naturalWidth, image.naturalHeight
    ]))),
    selected: cells.map((cell) => cell.classList.contains('rc-imageselect-tileselected') || cell.classList.contains('rc-imageselect-dynamic-selected')),
    error: text('.rc-imageselect-incorrect-response, .rc-imageselect-error-select-more, .rc-imageselect-error-dynamic-more'),
    blocked, unsupported,
    visual_state: cells.map((cell) => ({
        classes: cell.className,
        images: [...cell.querySelectorAll('img')].map((image) => ({
            opacity: getComputedStyle(image).opacity,
            parent_opacity: getComputedStyle(image.parentElement).opacity,
            complete: image.complete
        })),
        overlays: [...cell.querySelectorAll('.rc-image-tile-overlay')].map((overlay) => ({
            display: getComputedStyle(overlay).display, opacity: getComputedStyle(overlay).opacity
        }))
    }))
};

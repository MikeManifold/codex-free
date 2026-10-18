/** Keep upstream MCP media as media; never JSON-stringify it into text. */
export function forwardMcpResult(response, { compactCuaDocs = false } = {}) {
  if (!response || typeof response !== 'object' || !Array.isArray(response.content)) {
    return {
      content: [{ type: 'text', text: 'Upstream did not return a valid MCP tool result.' }],
      isError: true,
    };
  }

  const content = compactCuaDocs
    ? response.content.filter((block, _index, blocks) => !(
      blocks.length > 1
      && block?.type === 'text'
      && typeof block.text === 'string'
      && block.text.startsWith('## Computer Use')
    ))
    : response.content;

  // Preserve error state, annotations, structured data, and metadata from the
  // upstream response without adding a second copy of content in JSON.
  return { ...response, content };
}

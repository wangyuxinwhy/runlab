export function mergeConfig(base, overlay) {
  return { ...base, ...overlay };
}

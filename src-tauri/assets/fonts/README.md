# Baseline UI fonts

Sans: Geist 400/500/600 (Orca's UI font) from the `geist` npm package 1.9.2 static TTFs, with name IDs 1/2 normalized to one `Geist` family (IDs 16/17 dropped) so native matching selects weights; outlines and metrics unchanged. Mono stays JetBrains Mono: Orca's `SF Mono` is not exposed to apps through CoreText.

Earlier note (JetBrains Mono provenance):

These Latin subsets come from the same Fontsource WOFF files used by the Tauri baseline (`68e4ddf`): Inter 400/500/600 and JetBrains Mono 400. They were converted to uncompressed TrueType with fontTools. Inter family/style names were normalized to one family so native font matching selects the same CSS weights; glyph outlines and metrics are unchanged. Licenses accompany these fonts and are copied into app/installer resources.

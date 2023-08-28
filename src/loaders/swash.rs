// font-kit/src/loaders/swash.rs
//
// Copyright © 2018 The Pathfinder Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A loader that uses swash API to load and rasterize fonts.

use log::warn;
use pathfinder_geometry::rect::{RectF, RectI};
use pathfinder_geometry::transform2d::Transform2F;
use pathfinder_geometry::vector::Vector2F;
use std::f32;
use std::fmt::{self, Debug, Formatter};
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use crate::canvas::{Canvas, RasterizationOptions};
use crate::error::{FontLoadingError, GlyphLoadingError};
use crate::file_type::FileType;
use crate::handle::Handle;
use crate::hinting::HintingOptions;
use crate::loader::{FallbackResult, Loader};
use crate::metrics::Metrics;
use crate::outline::OutlineSink;
use crate::properties::{Properties, Stretch, Style, Weight};
use crate::utils;

/// A loader that uses Apple's Core Text API to load and rasterize fonts.
#[derive(Clone)]
pub struct Font {
    data: Arc<Vec<u8>>,
    // Offset to the table directory
    offset: u32,
    // Cache key
    key: swash::CacheKey,
}

/// Core Text's representation of a font.
pub type NativeFont = Font;

impl Font {
    /// Loads a font from raw font data (the contents of a `.ttf`/`.otf`/etc. file).
    pub fn from_bytes(data: Arc<Vec<u8>>, index: u32) -> Result<Font, FontLoadingError> {
        // Create a temporary font reference for the first font in the file.
        // This will do some basic validation, compute the necessary offset
        // and generate a fresh cache key for us.
        if let Some(font) = swash::FontRef::from_index(&data, index as usize) {
            let (offset, key) = (font.offset, font.key);
            // Return our struct with the original file data and copies of the
            // offset and key from the font reference
            return Ok(Self { data, offset, key });
        };
        return Err(FontLoadingError::Parse);
    }

    /// Loads a font from a `.ttf`/`.otf`/etc. file.
    ///
    /// If the file is a collection (`.ttc`/`.otc`/etc.), `font_index` specifies the index of the
    /// font to load from it. If the file represents a single font, pass 0 for `font_index`.
    pub fn from_file(file: &mut File, font_index: u32) -> Result<Font, FontLoadingError> {
        file.seek(SeekFrom::Start(0))?;
        let font_data = Arc::new(utils::slurp_file(file).map_err(FontLoadingError::Io)?);
        Font::from_bytes(font_data, font_index)
    }

    /// Loads a font from the path to a `.ttf`/`.otf`/etc. file.
    ///
    /// If the file is a collection (`.ttc`/`.otc`/etc.), `font_index` specifies the index of the
    /// font to load from it. If the file represents a single font, pass 0 for `font_index`.
    #[inline]
    pub fn from_path<P: AsRef<Path>>(path: P, font_index: u32) -> Result<Font, FontLoadingError> {
        <Font as Loader>::from_path(path, font_index)
    }

    /// Creates a font from a native API handle.
    pub unsafe fn from_native_font(_font: NativeFont) -> Font {
        unimplemented!()
    }

    /// Loads the font pointed to by a handle.
    #[inline]
    pub fn from_handle(handle: &Handle) -> Result<Self, FontLoadingError> {
        <Self as Loader>::from_handle(handle)
    }

    // Create the transient font reference for accessing this crate's
    // functionality.
    fn as_ref(&self) -> swash::FontRef {
        // Note that you'll want to initialize the struct directly here as
        // using any of the FontRef constructors will generate a new key which,
        // while completely safe, will nullify the performance optimizations of
        // the caching mechanisms used in this crate.
        swash::FontRef {
            data: &self.data,
            offset: self.offset,
            key: self.key,
        }
    }

    /// Determines whether a file represents a supported font, and if so, what type of font it is.
    pub fn analyze_bytes(_data: Arc<Vec<u8>>) -> Result<FileType, FontLoadingError> {
        todo!()
    }

    /// Determines whether a file represents a supported font, and if so, what type of font it is.
    pub fn analyze_file(_file: &mut File) -> Result<FileType, FontLoadingError> {
        todo!()
    }

    /// Determines whether a path points to a supported font, and if so, what type of font it is.
    #[inline]
    pub fn analyze_path<P: AsRef<Path>>(path: P) -> Result<FileType, FontLoadingError> {
        <Self as Loader>::analyze_path(path)
    }

    /// Returns the wrapped native font handle.
    #[inline]
    pub fn native_font(&self) -> NativeFont {
        unimplemented!()
    }

    fn find_localized_string(&self, id: swash::StringId) -> Option<String> {
        self.as_ref()
            .localized_strings()
            .find_by_id(id, None)
            .and_then(|s| Some(s.to_string()))
    }

    /// Returns the PostScript name of the font. This should be globally unique.
    #[inline]
    pub fn postscript_name(&self) -> Option<String> {
        self.find_localized_string(swash::StringId::PostScript)
    }

    /// Returns the full name of the font (also known as "display name" on macOS).
    #[inline]
    pub fn full_name(&self) -> String {
        self.find_localized_string(swash::StringId::Full)
            .expect("Full name not available")
    }

    /// Returns the name of the font family.
    #[inline]
    pub fn family_name(&self) -> String {
        self.find_localized_string(swash::StringId::Family)
            .expect("Family name not available")
    }

    /// Returns true if and only if the font is monospace (fixed-width).
    #[inline]
    pub fn is_monospace(&self) -> bool {
        self.as_ref().metrics(&[]).is_monospace
    }

    /// Returns the values of various font properties, corresponding to those defined in CSS.
    pub fn properties(&self) -> Properties {
        let (stretch, weight, style) = self.as_ref().attributes().parts();

        Properties {
            stretch: Stretch(stretch.to_percentage()),
            weight: Weight(weight.0 as f32),
            style: match style {
                swash::Style::Normal => Style::Normal,
                swash::Style::Italic => Style::Italic,
                swash::Style::Oblique(_) => Style::Oblique,
            },
        }
    }

    /// Returns the number of glyphs in the font.
    ///
    /// Glyph IDs range from 0 inclusive to this value exclusive.
    pub fn glyph_count(&self) -> u32 {
        unimplemented!()
    }

    /// Returns the usual glyph ID for a Unicode character.
    ///
    /// Be careful with this function; typographically correct character-to-glyph mapping must be
    /// done using a *shaper* such as HarfBuzz. This function is only useful for best-effort simple
    /// use cases like "what does character X look like on its own".
    pub fn glyph_for_char(&self, _c: char) -> Option<u32> {
        unimplemented!()
    }

    /// Returns the glyph ID for the specified glyph name.
    #[inline]
    pub fn glyph_by_name(&self, _name: &str) -> Option<u32> {
        unimplemented!()
    }

    /// Sends the vector path for a glyph to a path builder.
    ///
    /// If `hinting_mode` is not None, this function performs grid-fitting as requested before
    /// sending the hinding outlines to the builder.
    ///
    /// TODO(pcwalton): What should we do for bitmap glyphs?
    pub fn outline<S>(
        &self,
        _glyph_id: u32,
        _: HintingOptions,
        _sink: &mut S,
    ) -> Result<(), GlyphLoadingError>
    where
        S: OutlineSink,
    {
        unimplemented!()
    }

    /// Returns the boundaries of a glyph in font units.
    pub fn typographic_bounds(&self, _glyph_id: u32) -> Result<RectF, GlyphLoadingError> {
        unimplemented!()
    }

    /// Returns the distance from the origin of the glyph with the given ID to the next, in font
    /// units.
    pub fn advance(&self, _glyph_id: u32) -> Result<Vector2F, GlyphLoadingError> {
        unimplemented!()
    }

    /// Returns the amount that the given glyph should be displaced from the origin.
    pub fn origin(&self, _glyph_id: u32) -> Result<Vector2F, GlyphLoadingError> {
        unimplemented!()
    }

    /// Retrieves various metrics that apply to the entire font.
    pub fn metrics(&self) -> Metrics {
        unimplemented!()
    }

    /// Returns a handle to this font, if possible.
    ///
    /// This is useful if you want to open the font with a different loader.
    #[inline]
    pub fn handle(&self) -> Option<Handle> {
        <Self as Loader>::handle(self)
    }

    /// Attempts to return the raw font data (contents of the font file).
    ///
    /// If this font is a member of a collection, this function returns the data for the entire
    /// collection.
    pub fn copy_font_data(&self) -> Option<Arc<Vec<u8>>> {
        Some(self.data.clone())
    }

    /// Returns the pixel boundaries that the glyph will take up when rendered using this loader's
    /// rasterizer at the given size and transform.
    #[inline]
    pub fn raster_bounds(
        &self,
        _glyph_id: u32,
        _point_size: f32,
        _transform: Transform2F,
        _hinting_options: HintingOptions,
        _rasterization_options: RasterizationOptions,
    ) -> Result<RectI, GlyphLoadingError> {
        unimplemented!()
    }

    /// Rasterizes a glyph to a canvas with the given size and origin.
    ///
    /// Format conversion will be performed if the canvas format does not match the rasterization
    /// options. For example, if bilevel (black and white) rendering is requested to an RGBA
    /// surface, this function will automatically convert the 1-bit raster image to the 32-bit
    /// format of the canvas. Note that this may result in a performance penalty, depending on the
    /// loader.
    ///
    /// If `hinting_options` is not None, the requested grid fitting is performed.
    ///
    /// TODO(pcwalton): This is woefully incomplete. See WebRender's code for a more complete
    /// implementation.
    pub fn rasterize_glyph(
        &self,
        _canvas: &mut Canvas,
        _glyph_id: u32,
        _point_size: f32,
        _transform: Transform2F,
        _hinting_options: HintingOptions,
        _rasterization_options: RasterizationOptions,
    ) -> Result<(), GlyphLoadingError> {
        unimplemented!()
    }

    /// Returns true if and only if the font loader can perform hinting in the requested way.
    ///
    /// Some APIs support only rasterizing glyphs with hinting, not retriving hinted outlines. If
    /// `for_rasterization` is false, this function returns true if and only if the loader supports
    /// retrieval of hinted *outlines*. If `for_rasterization` is true, this function returns true
    /// if and only if the loader supports *rasterizing* hinted glyphs.
    #[inline]
    pub fn supports_hinting_options(&self, _hinting_options: HintingOptions, _: bool) -> bool {
        unimplemented!()
    }

    /// Get font fallback results for the given text and locale.
    ///
    /// Note: this is currently just a stub implementation, a proper implementation
    /// would use CTFontCopyDefaultCascadeListForLanguages.
    fn get_fallbacks(&self, text: &str, _locale: &str) -> FallbackResult<Font> {
        warn!("unsupported");
        FallbackResult {
            fonts: Vec::new(),
            valid_len: text.len(),
        }
    }

    /// Returns the raw contents of the OpenType table with the given tag.
    ///
    /// Tags are four-character codes. A list of tags can be found in the [OpenType specification].
    ///
    /// [OpenType specification]: https://docs.microsoft.com/en-us/typography/opentype/spec/
    #[inline]
    pub fn load_font_table(&self, _table_tag: u32) -> Option<Box<[u8]>> {
        unimplemented!()
    }
}

impl Loader for Font {
    type NativeFont = NativeFont;

    #[inline]
    fn from_bytes(font_data: Arc<Vec<u8>>, font_index: u32) -> Result<Self, FontLoadingError> {
        Font::from_bytes(font_data, font_index)
    }

    #[inline]
    fn from_file(file: &mut File, font_index: u32) -> Result<Font, FontLoadingError> {
        Font::from_file(file, font_index)
    }

    #[inline]
    unsafe fn from_native_font(native_font: Self::NativeFont) -> Self {
        Font::from_native_font(native_font)
    }

    #[inline]
    fn analyze_bytes(font_data: Arc<Vec<u8>>) -> Result<FileType, FontLoadingError> {
        Font::analyze_bytes(font_data)
    }

    #[inline]
    fn analyze_file(file: &mut File) -> Result<FileType, FontLoadingError> {
        Font::analyze_file(file)
    }

    #[inline]
    fn native_font(&self) -> Self::NativeFont {
        self.native_font()
    }

    #[inline]
    fn postscript_name(&self) -> Option<String> {
        self.postscript_name()
    }

    #[inline]
    fn full_name(&self) -> String {
        self.full_name()
    }

    #[inline]
    fn family_name(&self) -> String {
        self.family_name()
    }

    #[inline]
    fn is_monospace(&self) -> bool {
        self.is_monospace()
    }

    #[inline]
    fn properties(&self) -> Properties {
        self.properties()
    }

    #[inline]
    fn glyph_for_char(&self, character: char) -> Option<u32> {
        self.glyph_for_char(character)
    }

    #[inline]
    fn glyph_by_name(&self, name: &str) -> Option<u32> {
        self.glyph_by_name(name)
    }

    #[inline]
    fn glyph_count(&self) -> u32 {
        self.glyph_count()
    }

    #[inline]
    fn outline<S>(
        &self,
        glyph_id: u32,
        hinting_mode: HintingOptions,
        sink: &mut S,
    ) -> Result<(), GlyphLoadingError>
    where
        S: OutlineSink,
    {
        self.outline(glyph_id, hinting_mode, sink)
    }

    #[inline]
    fn typographic_bounds(&self, glyph_id: u32) -> Result<RectF, GlyphLoadingError> {
        self.typographic_bounds(glyph_id)
    }

    #[inline]
    fn advance(&self, glyph_id: u32) -> Result<Vector2F, GlyphLoadingError> {
        self.advance(glyph_id)
    }

    #[inline]
    fn origin(&self, glyph_id: u32) -> Result<Vector2F, GlyphLoadingError> {
        self.origin(glyph_id)
    }

    #[inline]
    fn metrics(&self) -> Metrics {
        self.metrics()
    }

    #[inline]
    fn copy_font_data(&self) -> Option<Arc<Vec<u8>>> {
        self.copy_font_data()
    }

    #[inline]
    fn supports_hinting_options(
        &self,
        hinting_options: HintingOptions,
        for_rasterization: bool,
    ) -> bool {
        self.supports_hinting_options(hinting_options, for_rasterization)
    }

    #[inline]
    fn rasterize_glyph(
        &self,
        canvas: &mut Canvas,
        glyph_id: u32,
        point_size: f32,
        transform: Transform2F,
        hinting_options: HintingOptions,
        rasterization_options: RasterizationOptions,
    ) -> Result<(), GlyphLoadingError> {
        self.rasterize_glyph(
            canvas,
            glyph_id,
            point_size,
            transform,
            hinting_options,
            rasterization_options,
        )
    }

    #[inline]
    fn get_fallbacks(&self, text: &str, locale: &str) -> FallbackResult<Self> {
        self.get_fallbacks(text, locale)
    }

    #[inline]
    fn load_font_table(&self, table_tag: u32) -> Option<Box<[u8]>> {
        self.load_font_table(table_tag)
    }
}

impl Debug for Font {
    fn fmt(&self, fmt: &mut Formatter) -> Result<(), fmt::Error> {
        self.full_name().fmt(fmt)
    }
}

#[cfg(test)]
mod test {}

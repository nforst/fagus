//! The cascade: from matched rules to computed styles.

use std::sync::Arc;

use fagus_css::matching::{Ancestors, PseudoElementKind};
use fagus_css::{CascadeBand, Origin as RuleOrigin, Precedence, RuleSet};
use fagus_dom::{Document, NodeId};
use lightningcss::properties::custom::{
    CustomProperty, CustomPropertyName, Token, TokenList, TokenOrValue, UnparsedProperty,
};
use lightningcss::properties::{Property, PropertyId};
use lightningcss::stylesheet::StyleAttribute;
use lightningcss::traits::IntoOwned;
use lightningcss::values::color::CssColor;
use lightningcss::values::length::LengthValue;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::background;
use crate::compute::{self, Context};
use crate::functions::Functions;
use crate::style::{ComputedStyle, ComputedValues};
use crate::values::{
    Display, Float, FontMetrics, FontRequest, FontSizeBasis, FontStyle, FontUnits, Position,
};
use crate::variables::{self, Registry};
use crate::zoom;

mod animation;
mod interpolate;

/// A CSS-wide keyword.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Keyword {
    Inherit,
    Initial,
    Unset,
}

/// The winning declaration of a property.
#[derive(Clone)]
enum Declared<'a> {
    /// A value of a rule, or a longhand of one expanded beforehand.
    Value(&'a Property<'static>),
    /// A value made for this element (a longhand of an inline style, a repaired gradient).
    Owned(Box<Property<'static>>),
    Keyword(Keyword),
    /// A declaration with `var()`: substituted and parsed when the custom properties are known.
    Variable(&'a UnparsedProperty<'static>),
}

/// The declared values of one element, in cascade order (the last declaration of a property wins).
#[derive(Default)]
struct Declarations<'a> {
    entries: Vec<Option<(PropertyId<'static>, Declared<'a>)>>,
    index: FxHashMap<PropertyId<'static>, usize>,
    custom: Vec<(String, &'a TokenList<'static>)>,
}

/// Presentational hints parsed once for all elements that have the same.
#[derive(Default)]
struct Hints(FxHashMap<String, Option<Arc<StyleAttribute<'static>>>>);

impl Hints {
    /// The presentational hints of an element, if it has any.
    fn of(&mut self, document: &Document, element: NodeId) -> Option<Arc<StyleAttribute<'static>>> {
        let css = crate::hints::hints(document, element);
        if css.is_empty() {
            return None;
        }
        self.0
            .entry(css)
            .or_insert_with_key(|css| fagus_css::parse_declarations(css).map(Arc::new))
            .clone()
    }
}

/// The longhands of the shorthands in a rule set, expanded once instead of for every element.
#[derive(Default)]
struct Expansions {
    longhands: FxHashMap<usize, Vec<Property<'static>>>,
    /// Whether values refer to the elements they apply to (`attr()`, `sibling-index()`), so that
    /// elements with the same declarations do not share their styles.
    element_dependent: bool,
}

impl Expansions {
    fn new(rules: &RuleSet) -> Self {
        let mut longhands = FxHashMap::default();
        let mut element_dependent = false;
        for rule in rules.rules() {
            let block = &rule.declarations;
            for property in block.declarations.iter().chain(&block.important_declarations) {
                if is_plain_shorthand(property) {
                    longhands
                        .entry(std::ptr::from_ref(property) as usize)
                        .or_insert_with(|| expand(property));
                }
                element_dependent = element_dependent || depends_on_element(property);
            }
        }
        Self {
            longhands,
            element_dependent,
        }
    }

    fn get(&self, property: &Property<'static>) -> Option<&[Property<'static>]> {
        self.longhands
            .get(&(std::ptr::from_ref(property) as usize))
            .map(Vec::as_slice)
    }
}

/// Whether a declaration refers to the element it applies to: with `attr()` of a type,
/// `sibling-index()` or `sibling-count()`.
fn depends_on_element(property: &Property<'static>) -> bool {
    fn tokens(list: &TokenList<'_>) -> bool {
        list.0.iter().any(|token| match token {
            TokenOrValue::Function(function) => {
                let name = function.name.0.as_ref();
                name.eq_ignore_ascii_case("sibling-index")
                    || name.eq_ignore_ascii_case("sibling-count")
                    || (name.eq_ignore_ascii_case("attr") && fagus_css::is_typed_attr(&function.arguments))
                    || tokens(&function.arguments)
            }
            TokenOrValue::Var(variable) => variable.fallback.as_ref().is_some_and(tokens),
            _ => false,
        })
    }
    match property {
        Property::Unparsed(unparsed) => tokens(&unparsed.value),
        Property::Custom(custom) => tokens(&custom.value),
        _ => false,
    }
}

/// A shorthand with a value of its own (not `var()`, not a keyword kept as tokens).
fn is_plain_shorthand(property: &Property<'static>) -> bool {
    !matches!(
        property,
        Property::Custom(_) | Property::Unparsed(_) | Property::All(_)
    ) && property.property_id().longhands().is_some()
}

impl<'a> Declarations<'a> {
    /// Drops the declarations of the properties that `keep` rejects.
    fn retain(&mut self, keep: impl Fn(&PropertyId<'static>) -> bool) {
        for entry in &mut self.entries {
            if entry.as_ref().is_some_and(|(id, _)| !keep(id)) {
                *entry = None;
            }
        }
        self.index.retain(|id, _| keep(id));
    }

    /// Drops the declaration of a property: it is not declared (for a logical property, its
    /// physical one may be).
    fn remove(&mut self, id: &PropertyId<'static>) {
        if let Some(previous) = self.index.remove(id) {
            self.entries[previous] = None;
        }
    }

    fn set(&mut self, id: PropertyId<'static>, declared: Declared<'a>) {
        if let Some(previous) = self.index.insert(id.clone(), self.entries.len()) {
            self.entries[previous] = None;
        }
        self.entries.push(Some((id, declared)));
    }

    fn add(&mut self, property: &'a Property<'static>, expansions: &'a Expansions) {
        match property {
            Property::Custom(custom) => match &custom.name {
                CustomPropertyName::Custom(name) => self.custom.push((name.0.to_string(), &custom.value)),
                CustomPropertyName::Unknown(name) if rejected(name.0.as_ref(), &custom.value) => {}
                CustomPropertyName::Unknown(_) => match keyword(&custom.value) {
                    Some(keyword) => self.set(cascade_id(property), Declared::Keyword(keyword)),
                    None => self.set(cascade_id(property), Declared::Value(property)),
                },
            },
            Property::Unparsed(unparsed)
                if !variables::has_variables(&unparsed.value)
                    && fagus_css::gradient::needs_repair(&unparsed.value) =>
            {
                for longhand in background::repaired_longhands(&unparsed.property_id, &unparsed.value)
                    .into_iter()
                    .flatten()
                {
                    self.set(cascade_id(&longhand), Declared::Owned(Box::new(longhand)));
                }
            }
            // A `text-emphasis-position` that is not valid (`left` alone, `over over`) is
            // dropped as Chrome drops it, so that an earlier declaration applies.
            Property::Unparsed(unparsed)
                if matches!(unparsed.property_id, PropertyId::TextEmphasisPosition(_))
                    && !variables::has_variables(&unparsed.value)
                    && keyword(&unparsed.value).is_none() => {}
            Property::Unparsed(unparsed) => {
                let declared = keyword(&unparsed.value);
                for id in longhand_ids(&unparsed.property_id) {
                    match declared {
                        Some(keyword) => self.set(id, Declared::Keyword(keyword)),
                        None => self.set(id, Declared::Variable(unparsed)),
                    }
                }
            }
            Property::All(wide) => {
                let keyword = match wide {
                    lightningcss::properties::CSSWideKeyword::Inherit => Keyword::Inherit,
                    lightningcss::properties::CSSWideKeyword::Initial => Keyword::Initial,
                    _ => Keyword::Unset,
                };
                // `all` resets every property except custom properties, `direction` and
                // `unicode-bidi` (CSS Cascade 4, §3.2).
                let ids: Vec<_> = self.index.keys().filter(|id| all_resets(id)).cloned().collect();
                for id in ids {
                    self.set(id, Declared::Keyword(keyword));
                }
            }
            // lightningcss reads `font-family: inherit` (and the other CSS-wide keywords) as a
            // generic family: it is the keyword.
            Property::FontFamily(families) if let Some(keyword) = family_keyword(families) => {
                self.set(PropertyId::FontFamily, Declared::Keyword(keyword));
            }
            _ if !is_plain_shorthand(property) => self.set(cascade_id(property), Declared::Value(property)),
            _ => match expansions.get(property) {
                Some(longhands) => {
                    for longhand in longhands {
                        self.set(cascade_id(longhand), Declared::Value(longhand));
                    }
                }
                None => {
                    for longhand in expand(property) {
                        self.set(cascade_id(&longhand), Declared::Owned(Box::new(longhand)));
                    }
                }
            },
        }
    }
}

/// Whether the shorthand `all` sets a property: not `direction` and `unicode-bidi`, nor custom
/// properties.
fn all_resets(id: &PropertyId<'_>) -> bool {
    !matches!(
        id,
        PropertyId::Direction | PropertyId::UnicodeBidi | PropertyId::Custom(CustomPropertyName::Custom(_))
    )
}

/// How a declaration rolls the cascade back: `revert` to the origin before its own, as if
/// author style did not exist; `revert-layer` to the cascade layer before its own.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rollback {
    Origin,
    Layer,
}

/// Whether a declaration is `revert` or `revert-layer` alone.
fn rollback(property: &Property<'static>) -> Option<Rollback> {
    use lightningcss::properties::CSSWideKeyword;
    use lightningcss::properties::font::{FontFamily, GenericFontFamily};
    let of_tokens = |tokens: &TokenList<'_>| {
        let mut significant = tokens
            .0
            .iter()
            .filter(|token| !matches!(token, TokenOrValue::Token(Token::WhiteSpace(_))));
        match (significant.next(), significant.next()) {
            (Some(TokenOrValue::Token(Token::Ident(ident))), None) => {
                if ident.eq_ignore_ascii_case("revert") {
                    Some(Rollback::Origin)
                } else if ident.eq_ignore_ascii_case("revert-layer") {
                    Some(Rollback::Layer)
                } else {
                    None
                }
            }
            _ => None,
        }
    };
    match property {
        Property::Unparsed(unparsed) => of_tokens(&unparsed.value),
        // Unknown properties kept as tokens, and custom properties (`--x: revert-layer`).
        Property::Custom(CustomProperty { value, .. }) => of_tokens(value),
        Property::All(CSSWideKeyword::Revert) => Some(Rollback::Origin),
        Property::All(CSSWideKeyword::RevertLayer) => Some(Rollback::Layer),
        Property::FontFamily(families) => match families.as_slice() {
            [FontFamily::Generic(GenericFontFamily::Revert)] => Some(Rollback::Origin),
            [FontFamily::Generic(GenericFontFamily::RevertLayer)] => Some(Rollback::Layer),
            _ => None,
        },
        _ => None,
    }
}

/// Where a declaration's origin and cascade layer rank, whether it is important or not: the
/// user agent's below the author's, a layer's in the order of normal declarations, and the
/// `style` attribute above all layers.
fn layer_rank(precedence: &Precedence) -> (bool, u64) {
    let layer = match precedence.band {
        CascadeBand::InlineNormal | CascadeBand::InlineImportant => u64::from(u32::MAX) + 1,
        CascadeBand::AuthorImportant | CascadeBand::UserAgentImportant => {
            u64::from(u32::MAX - precedence.layer)
        }
        CascadeBand::AuthorNormal | CascadeBand::UserAgentNormal => u64::from(precedence.layer),
    };
    (is_author(precedence), layer)
}

/// Whether a declaration of rank `precedence` comes from the author (a rule, a presentational
/// hint or a `style` attribute).
fn is_author(precedence: &Precedence) -> bool {
    !matches!(
        precedence.band,
        CascadeBand::UserAgentNormal | CascadeBand::UserAgentImportant
    )
}

/// The declared values of an element from its declarations in cascade order (CSS Cascade 5,
/// §7.3): `revert` and `revert-layer` take the value the declarations before their origin or
/// cascade layer give, `unset` where there is none.
fn cascade_declarations<'a>(
    ordered: &[(Precedence, &'a Property<'static>)],
    expansions: &'a Expansions,
) -> Declarations<'a> {
    let mut declarations = Declarations::default();
    for (index, (precedence, property)) in ordered.iter().enumerate() {
        let Some(kind) = rollback(property) else {
            declarations.add(property, expansions);
            continue;
        };
        // User agent declarations have no earlier origin: `revert` is `unset` there.
        // `revert-layer` takes what the layers below its own give, normal and important
        // declarations alike, whether it is important or not (Blink's
        // `CascadePriority::ForLayerComparison`).
        let earlier = match kind {
            Rollback::Origin if !is_author(precedence) => cascade_declarations(&[], expansions),
            Rollback::Origin => {
                let start = ordered[..index]
                    .iter()
                    .position(|(other, _)| is_author(other))
                    .unwrap_or(index);
                cascade_declarations(&ordered[..start], expansions)
            }
            Rollback::Layer => {
                let below: Vec<(Precedence, &Property<'static>)> = ordered[..index]
                    .iter()
                    .filter(|(other, _)| layer_rank(other) < layer_rank(precedence))
                    .copied()
                    .collect();
                cascade_declarations(&below, expansions)
            }
        };
        let ids: Vec<PropertyId<'static>> = match property {
            Property::All(_) => declarations
                .index
                .keys()
                .chain(earlier.index.keys())
                .filter(|id| all_resets(id))
                .cloned()
                .collect(),
            Property::Unparsed(unparsed) => longhand_ids(&unparsed.property_id),
            _ => vec![cascade_id(property)],
        };
        // A property that nothing before declares is not declared: unset, and where it is a
        // logical property, its physical one keeps what the declarations before give it (a
        // control's border from the user agent's `border`).
        for id in ids {
            match earlier
                .index
                .get(&id)
                .and_then(|&position| earlier.entries[position].as_ref())
            {
                Some((_, declared)) => declarations.set(id, declared.clone()),
                None => declarations.remove(&id),
            }
        }
    }
    declarations
}

/// The property a declaration sets in the cascade. Background images that Fagus keeps as tokens
/// count as `background-image`.
fn cascade_id(property: &Property<'static>) -> PropertyId<'static> {
    match property {
        Property::Custom(CustomProperty {
            name: CustomPropertyName::Unknown(name),
            ..
        }) if name.0.as_ref() == background::IMAGE_TOKENS => PropertyId::BackgroundImage,
        // An alias: the later of the two declarations wins.
        Property::Custom(CustomProperty {
            name: CustomPropertyName::Unknown(name),
            ..
        }) if name.0.eq_ignore_ascii_case("-webkit-writing-mode") => writing_mode_id(),
        _ => property.property_id().into_owned(),
    }
}

/// A shorthand's longhands, recursively; a longhand on its own.
pub(crate) fn expand(property: &Property<'static>) -> Vec<Property<'static>> {
    match property.property_id().longhands() {
        Some(ids) => ids
            .iter()
            .filter_map(|id| property.longhand(id).or_else(|| missing_longhand(property, id)))
            .flat_map(|longhand| expand(&longhand.into_owned()))
            .chain(reset_longhands(property))
            .collect(),
        None => vec![property.clone()],
    }
}

/// Longhands that a shorthand resets to their initial values without a way to set them, where
/// lightningcss does not know them: `font` resets the other font properties (CSS Fonts 4, §2.8),
/// as in Chrome, and `border` the border image.
fn reset_longhands(property: &Property<'static>) -> Vec<Property<'static>> {
    // `border` resets `border-image` too (CSS Backgrounds 3, §4.4).
    if matches!(property, Property::Border(_)) {
        return expand(&Property::BorderImage(
            lightningcss::properties::border_image::BorderImage::default(),
            lightningcss::vendor_prefix::VendorPrefix::empty(),
        ));
    }
    if !matches!(property, Property::Font(_)) {
        return Vec::new();
    }
    [
        ("font-variant-ligatures", "normal"),
        ("font-variant-numeric", "normal"),
        ("font-variant-east-asian", "normal"),
        ("font-variant-position", "normal"),
        ("font-variant-alternates", "normal"),
        ("font-kerning", "auto"),
        ("font-feature-settings", "normal"),
        ("font-variation-settings", "normal"),
        ("font-optical-sizing", "auto"),
        ("font-size-adjust", "none"),
    ]
    .into_iter()
    .map(|(name, value)| {
        Property::Custom(CustomProperty {
            name: CustomPropertyName::Unknown(lightningcss::values::ident::Ident(name.into())),
            value: TokenList(vec![TokenOrValue::Token(Token::Ident(value.into()))]),
        })
    })
    .collect()
}

/// Longhands that lightningcss does not give out of their shorthand: the thickness in
/// `text-decoration`, a longhand without vendor prefixes in a shorthand with them.
fn missing_longhand(property: &Property<'static>, id: &PropertyId<'static>) -> Option<Property<'static>> {
    // lightningcss gives the longhands of a shorthand that may have a vendor prefix only where
    // they have the shorthand's prefix: the shorthand with theirs has them.
    match property {
        Property::TextDecoration(decoration, _) => {
            Property::TextDecoration(decoration.clone(), id.prefix()).longhand(id)
        }
        Property::BorderImage(image, _) => Property::BorderImage(image.clone(), id.prefix()).longhand(id),
        Property::Mask(masks, _) => Property::Mask(masks.clone(), id.prefix()).longhand(id),
        _ => None,
    }
}

fn longhand_ids(id: &PropertyId<'static>) -> Vec<PropertyId<'static>> {
    match id.longhands() {
        Some(ids) => ids.iter().flat_map(longhand_ids).collect(),
        None => vec![id.clone()],
    }
}

/// The CSS-wide keyword a token list consists of, if any.
fn keyword(tokens: &TokenList<'_>) -> Option<Keyword> {
    let mut significant = tokens
        .0
        .iter()
        .filter(|token| !matches!(token, TokenOrValue::Token(Token::WhiteSpace(_))));
    let first = significant.next()?;
    if significant.next().is_some() {
        return None;
    }
    let TokenOrValue::Token(Token::Ident(ident)) = first else {
        return None;
    };
    match ident.to_ascii_lowercase().as_str() {
        "inherit" => Some(Keyword::Inherit),
        "initial" => Some(Keyword::Initial),
        "unset" | "revert" | "revert-layer" => Some(Keyword::Unset),
        _ => None,
    }
}

/// The CSS-wide keyword that a `font-family` of one generic family is, as lightningcss parses
/// `inherit`, `initial`, `unset`, `revert` and `revert-layer` there.
fn family_keyword(families: &[lightningcss::properties::font::FontFamily<'_>]) -> Option<Keyword> {
    use lightningcss::properties::font::{FontFamily, GenericFontFamily};
    match families {
        [FontFamily::Generic(GenericFontFamily::Inherit)] => Some(Keyword::Inherit),
        [FontFamily::Generic(GenericFontFamily::Initial)] => Some(Keyword::Initial),
        [
            FontFamily::Generic(
                GenericFontFamily::Unset | GenericFontFamily::Revert | GenericFontFamily::RevertLayer,
            ),
        ] => Some(Keyword::Unset),
        _ => None,
    }
}

/// Whether Chrome drops a declaration of a property that lightningcss keeps as tokens as it
/// parses it, so that an earlier one applies: these properties take one of their keywords alone,
/// and `clip` takes `auto` or `rect()`; multi-column properties take valid counts and widths only.
fn rejected(name: &str, tokens: &TokenList<'static>) -> bool {
    const KEYWORDS: [(&str, &[&str]); 5] = [
        ("ruby-position", &["over", "under"]),
        ("scrollbar-width", &["auto", "thin", "none"]),
        (
            "-webkit-writing-mode",
            &["horizontal-tb", "vertical-rl", "vertical-lr"],
        ),
        (
            "text-spacing-trim",
            &["normal", "space-all", "space-first", "trim-start"],
        ),
        ("text-autospace", &["normal", "no-autospace"]),
    ];
    if keyword(tokens).is_some() || variables::has_variables(tokens) {
        return false;
    }
    if name.eq_ignore_ascii_case("clip") {
        return !crate::extra::is_clip(tokens);
    }
    if name.eq_ignore_ascii_case("zoom") {
        return zoom::specified(tokens).is_none();
    }
    // `corner-shape` and its longhands: one to four shapes.
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("corner-") && lower.ends_with("-shape") {
        return !crate::extra::is_corner_shape(&lower, tokens);
    }
    if let Some(valid) = crate::extra::is_columns_value(&lower, tokens) {
        return !valid;
    }
    if lower == "text-fit" {
        return crate::extra::text_fit(&crate::extra::significant(tokens)).is_none();
    }
    if lower == "hanging-punctuation" {
        return crate::values::HangingPunctuation::from_tokens(tokens).is_none();
    }
    if lower == "scrollbar-gutter" {
        return crate::extra::scrollbar_gutter(&crate::extra::significant(tokens)).is_none();
    }
    let Some((_, valid)) = KEYWORDS
        .iter()
        .find(|(property, _)| name.eq_ignore_ascii_case(property))
    else {
        return false;
    };
    let mut significant = tokens
        .0
        .iter()
        .filter(|token| !matches!(token, TokenOrValue::Token(Token::WhiteSpace(_))));
    let valid = match (significant.next(), significant.next()) {
        (Some(TokenOrValue::Token(Token::Ident(ident))), None) => {
            valid.iter().any(|keyword| ident.eq_ignore_ascii_case(keyword))
        }
        _ => false,
    };
    !valid
}

/// Computed styles of all elements of a document, and of their `::before` and `::after`.
#[derive(Debug)]
pub struct StyledDocument {
    styles: Vec<Option<Arc<ComputedStyle>>>,
    pseudo: FxHashMap<(NodeId, PseudoKey), Arc<ComputedStyle>>,
    /// The named strings that the descendants of each element with `display: none` set (see
    /// [`StyledDocument::hidden_string_sets`]).
    hidden_strings: FxHashMap<NodeId, Vec<(NodeId, StringSet)>>,
    /// The styles, charged to the budget of the render while they live.
    held: fagus_dom::budget::Held,
}

/// The value of `string-set`: the names with the content of each.
pub type StringSet = Vec<(String, Vec<crate::values::ContentItem>)>;

/// The pseudo-elements with their own style.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PseudoKey {
    Before,
    After,
    Marker,
    /// The marker of a `::before` that is a list item, inheriting from it.
    BeforeMarker,
    /// The marker of an `::after` that is a list item, inheriting from it.
    AfterMarker,
    /// `::first-line`, inheriting from the element.
    FirstLine,
    /// `::first-letter`, inheriting from `::first-line` where the element has one.
    FirstLetter,
    /// `::-webkit-progress-bar` of a `<progress>`, inheriting from the element.
    ProgressBar,
    /// `::-webkit-progress-value` of a `<progress>`, inheriting from its bar.
    ProgressValue,
    /// `::-webkit-meter-bar` of a `<meter>`, inheriting from the element.
    MeterBar,
    /// The value of a `<meter>`, inheriting from its bar: `::-webkit-meter-optimum-value`,
    /// `-suboptimum-value` or `-even-less-good-value`, as good as the value is.
    MeterValue,
    /// `::placeholder` of a text field or text area with a placeholder, inheriting from the
    /// element.
    Placeholder,
    /// `::-webkit-slider-runnable-track` of a slider that author rules style, inheriting from
    /// the element.
    SliderTrack,
    /// `::-webkit-slider-thumb` of a slider that author rules style, inheriting from its track.
    SliderThumb,
    /// `::scroll-marker`, inheriting from the element.
    ScrollMarker,
    /// `::scroll-marker-group` of a scroll container, inheriting from it.
    ScrollMarkerGroup,
    /// `::scroll-button()` in a direction, inheriting from the element.
    ScrollButton(fagus_css::matching::ScrollDirection),
    /// `::column::scroll-marker` of a multi-column container, inheriting from the element.
    ColumnScrollMarker,
    /// `::footnote-call` of a footnote, inheriting from the element.
    FootnoteCall,
    /// `::footnote-marker` of a footnote, inheriting from the element.
    FootnoteMarker,
}

impl StyledDocument {
    pub fn style(&self, node: NodeId) -> Option<&Arc<ComputedStyle>> {
        self.styles.get(node.index()).and_then(Option::as_ref)
    }

    pub fn pseudo(&self, node: NodeId, pseudo: PseudoKey) -> Option<&Arc<ComputedStyle>> {
        self.pseudo.get(&(node, pseudo))
    }

    /// The styles of all elements and pseudo-elements.
    pub fn all(&self) -> impl Iterator<Item = &Arc<ComputedStyle>> {
        self.styles.iter().flatten().chain(self.pseudo.values())
    }

    /// The descendants of `hidden`, an element with `display: none`, that set named strings,
    /// in document order, each with its `string-set`. They have no styles otherwise: the
    /// content of an element with `display: none` is not rendered. CSS Generated Content for
    /// Paged Media 3 (§1.1.1) has such an element assign its strings where its box would have
    /// been and does not say whether the elements in it do; here they do, in the same place.
    pub fn hidden_string_sets(&self, hidden: NodeId) -> &[(NodeId, StringSet)] {
        self.hidden_strings.get(&hidden).map_or(&[], Vec::as_slice)
    }
}

/// Whether a rule or a `style` attribute declares `string-set`: only then are the elements in
/// one with `display: none` styled, for the strings they set.
fn sets_strings(document: &Document, rules: &RuleSet) -> bool {
    let declares = |block: &fagus_css::DeclarationBlock| {
        block
            .declarations
            .iter()
            .chain(&block.important_declarations)
            .any(|property| property.property_id().name() == "string-set")
    };
    // (The rules of a selector list share their declarations.)
    let mut last = std::ptr::null();
    rules.rules().iter().any(|rule| {
        let block = Arc::as_ptr(&rule.declarations);
        !std::ptr::eq(std::mem::replace(&mut last, block), block) && declares(&rule.declarations)
    }) || document.descendants(NodeId::DOCUMENT).any(|node| {
        document
            .element(node)
            .and_then(|data| data.attribute("style"))
            .is_some_and(|css| css.to_ascii_lowercase().contains("string-set"))
    })
}

/// Computes the styles of all elements.
pub fn compute_styles(document: &Document, rules: &RuleSet, viewport: (f32, f32)) -> StyledDocument {
    compute_styles_in(document, rules, viewport, &ContainerSizes::default())
}

/// The sizes of query containers, by element, as a layout found them.
pub type ContainerSizes = FxHashMap<NodeId, fagus_css::ContainerSize>;

/// Query containers with their styles, innermost last.
type Chain = Arc<[(NodeId, Arc<ComputedStyle>)]>;

/// The query containers around an element, innermost last, and the sizes known of them: which
/// `@container` rules apply to the element (CSS Conditional 5, §6).
struct Containers<'a> {
    /// The query containers around the element, outermost first: size containers and named
    /// elements.
    chain: &'a [(NodeId, Arc<ComputedStyle>)],
    sizes: &'a ContainerSizes,
    /// The style of the parent element (of the originating element, for a pseudo-element),
    /// which unnamed style queries ask.
    parent: &'a ComputedStyle,
    registry: &'a Registry,
}

impl Containers<'_> {
    /// The size of a container as laid out, without its zoom: in the pixels of its own lengths,
    /// which container queries and container query units refer to.
    fn unzoomed(&self, node: NodeId, style: &ComputedValues) -> Option<fagus_css::ContainerSize> {
        let size = self.sizes.get(&node)?;
        let zoom = style.zoom;
        Some(fagus_css::ContainerSize {
            width: size.width / zoom,
            height: size.height.map(|height| height / zoom),
            font_size: size.font_size / zoom,
        })
    }

    /// What container query units refer to: the content box of the nearest container whose
    /// size is known, its height that of the page area unless it is a size container. Both are
    /// without zoom: the units multiply them by the zoom of the element, as viewport units.
    fn basis(&self, viewport: (f32, f32)) -> (f32, f32) {
        self.chain
            .iter()
            .rev()
            .find_map(|(node, style)| self.unzoomed(*node, style))
            .map_or(viewport, |size| (size.width, size.height.unwrap_or(viewport.1)))
    }

    /// Whether the container queries a rule is in hold. Each asks the nearest container of its
    /// name (or the nearest of all) that answers it: a query of sizes a size container, which
    /// must have a size known; a query of styles alone any element, the parent without a name.
    fn admit(&self, rule: &fagus_css::Rule) -> bool {
        rule.container.as_ref().is_none_or(|queries| {
            queries.iter().all(|query| {
                let sized = query.condition.as_ref().is_none_or(fagus_css::asks_size);
                let named = |style: &ComputedValues| {
                    query
                        .name
                        .as_ref()
                        .is_none_or(|name| style.container_names.iter().any(|own| own == name))
                };
                let container = if sized || query.name.is_some() {
                    self.chain
                        .iter()
                        .rev()
                        .find(|(_, style)| {
                            named(style)
                                && (!sized || style.container_type != crate::values::ContainerType::Normal)
                        })
                        .map(|(node, style)| (Some(*node), &**style))
                } else {
                    Some((None, self.parent))
                };
                let Some((node, style)) = container else {
                    return false;
                };
                let size = node.and_then(|node| self.unzoomed(node, style));
                if sized && size.is_none() {
                    return false;
                }
                fagus_css::container_matches(query, size, &|query| style_query(query, style, self.registry))
            })
        })
    }
}

/// Whether a container's custom properties answer a style query (`style(--name: value)`,
/// `style(--name)`): Chrome asks custom properties alone.
fn style_query(
    query: &lightningcss::rules::container::StyleQuery<'static>,
    container: &ComputedValues,
    registry: &Registry,
) -> bool {
    use lightningcss::media_query::Operator;
    use lightningcss::rules::container::StyleQuery;
    let lookup = |name: &str| {
        container
            .custom_properties
            .get(name)
            .map(AsRef::as_ref)
            .or_else(|| registry.initial(name))
    };
    match query {
        StyleQuery::Declaration(Property::Custom(CustomProperty {
            name: CustomPropertyName::Custom(name),
            value,
        })) => {
            let Some(actual) = lookup(name.0.as_ref()) else {
                return false;
            };
            let expected = variables::substitute(value, &lookup);
            // A property registered as a color compares as the absolute colors its values
            // compute to (`contrast-color(black)` matches `white`), as in Chrome.
            if registry.is_color(name.0.as_ref()) {
                let functions = Functions {
                    element: None,
                    media: None,
                    math: true,
                    lengths: None,
                };
                let color = |tokens: &TokenList<'static>| {
                    let resolved = variables::substitute_with(tokens, &lookup, &functions)?;
                    compute::color(&crate::color_functions::token_color(&resolved.0)?)
                };
                return expected.is_some_and(|expected| {
                    color(actual).is_some_and(|actual| Some(actual) == color(&expected))
                });
            }
            let text =
                |tokens: &TokenList<'static>| variables::to_css(tokens).map(|css| css.trim().to_owned());
            expected.is_some_and(|expected| text(actual).is_some() && text(actual) == text(&expected))
        }
        StyleQuery::Property(PropertyId::Custom(CustomPropertyName::Custom(name))) => {
            let name = name.0.as_ref();
            lookup(name).is_some_and(|value| registry.initial(name) != Some(value))
        }
        StyleQuery::Not(inner) => !style_query(inner, container, registry),
        StyleQuery::Operation { operator, conditions } => match operator {
            Operator::And => conditions
                .iter()
                .all(|condition| style_query(condition, container, registry)),
            Operator::Or => conditions
                .iter()
                .any(|condition| style_query(condition, container, registry)),
        },
        _ => false,
    }
}

/// Computes the styles of all elements, with `@container` rules answered by the sizes of the
/// containers given.
pub fn compute_styles_in(
    document: &Document,
    rules: &RuleSet,
    viewport: (f32, f32),
    sizes: &ContainerSizes,
) -> StyledDocument {
    compute_styles_with(document, rules, viewport, sizes, &crate::values::NoFontMetrics)
}

/// The metrics of fonts for font requests, as a [`FontMetrics`] gives them, kept for requests
/// asked before.
struct FontUnitsCache<'a> {
    fonts: &'a dyn FontMetrics,
    /// The metrics of fonts, by their properties, optical size and size (their bits).
    known: std::cell::RefCell<FxHashMap<UnitsKey, FontUnits>>,
    /// The line spacing of fonts at a size (its bits), for `line-height: normal`.
    spacing: std::cell::RefCell<FxHashMap<(FontKey, u32), Option<f32>>>,
}

/// The font properties that select a font: the families, the weight and stretch as bits, the
/// style and the language.
type FontKey = (Arc<[String]>, u32, FontStyle, u32, Option<Arc<str>>);

/// The font properties, the optical size and the size (their bits) that font metrics are kept by.
type UnitsKey = (FontKey, Option<u32>, u32);

impl<'a> FontUnitsCache<'a> {
    fn new(fonts: &'a dyn FontMetrics) -> Self {
        Self {
            fonts,
            known: std::cell::RefCell::default(),
            spacing: std::cell::RefCell::default(),
        }
    }

    /// The computed line height of a style in pixels, which `lh` refers to: `line-height:
    /// normal` is the line spacing of its first available font, 1.2 em without fonts.
    fn line_height(&self, style: &ComputedValues) -> f32 {
        let key = (
            (
                Arc::clone(&style.font_family),
                style.font_weight.to_bits(),
                style.font_style,
                style.font_stretch.to_bits(),
                style.lang.clone(),
            ),
            style.font_size.to_bits(),
        );
        let normal = if style.line_height == crate::values::LineHeight::Normal {
            let known = self.spacing.borrow().get(&key).copied();
            let spacing = known.unwrap_or_else(|| {
                let spacing = self.fonts.line_spacing(
                    &FontRequest {
                        families: Arc::clone(&style.font_family),
                        weight: style.font_weight,
                        style: style.font_style,
                        stretch: style.font_stretch,
                        optical_size: None,
                        size: style.font_size,
                        lang: style.lang.clone(),
                    },
                    style.font_size,
                );
                self.spacing.borrow_mut().insert(key, spacing);
                spacing
            });
            spacing.unwrap_or(style.font_size * 1.2)
        } else {
            style.font_size * 1.2
        };
        style.line_height_px(normal)
    }

    /// The metrics that the font-relative lengths of a style refer to: those of its first
    /// available font, scaled as `font-size-adjust` scales the font, as Chrome scales `ex`,
    /// `ch`, `cap` and `ic` (not `em`).
    fn adjusted_units(&self, style: &ComputedValues) -> FontUnits {
        use crate::values::{FontMetric, FontSizeAdjust};
        let units = style.font_units;
        let metric_of = |units: &FontUnits, metric: FontMetric| match metric {
            FontMetric::ExHeight => units.ex,
            FontMetric::CapHeight => units.cap,
            FontMetric::ChWidth => units.ch,
            FontMetric::IcWidth | FontMetric::IcHeight => units.ic,
        };
        let (metric, aspect) = match &style.font_size_adjust {
            FontSizeAdjust::None | FontSizeAdjust::FromFont(_, None) => return units,
            FontSizeAdjust::Aspect(metric, aspect) => (*metric, *aspect),
            FontSizeAdjust::FromFont(metric, Some(request)) => {
                (*metric, metric_of(&self.fonts.units(request), *metric))
            }
        };
        let own = metric_of(&units, metric);
        if !(own > 0.0 && aspect.is_finite()) {
            return units;
        }
        let factor = aspect / own;
        FontUnits {
            ex: units.ex * factor,
            ch: units.ch * factor,
            cap: units.cap * factor,
            ic: units.ic * factor,
            ch_upright: units.ch_upright.map(|ch| ch * factor),
            ..units
        }
    }

    /// The metrics of the first available font of a style.
    fn units(&self, style: &ComputedValues) -> FontUnits {
        let optical_size = optical_size(style);
        let key = (
            (
                Arc::clone(&style.font_family),
                style.font_weight.to_bits(),
                style.font_style,
                style.font_stretch.to_bits(),
                style.lang.clone(),
            ),
            optical_size.map(f32::to_bits),
            style.font_size.to_bits(),
        );
        if let Some(units) = self.known.borrow().get(&key) {
            return *units;
        }
        let units = self.fonts.units(&FontRequest {
            families: Arc::clone(&style.font_family),
            weight: style.font_weight,
            style: style.font_style,
            stretch: style.font_stretch,
            optical_size,
            size: style.font_size,
            lang: style.lang.clone(),
        });
        self.known.borrow_mut().insert(key, units);
        units
    }
}

/// The size on the `opsz` axis of a style's font: its font size, unless `font-optical-sizing`
/// is `none`.
fn optical_size(style: &ComputedValues) -> Option<f32> {
    (style.font_optical_sizing == crate::values::OpticalSizing::Auto).then_some(style.font_size)
}

/// What root-relative lengths refer to: the root element's font size, the metrics of its first
/// available font and its computed line height (`rem`, `rex`, `rch`, `rcap`, `ric`, `rlh`).
#[derive(Clone, Copy, Debug)]
struct RootFont {
    size: f32,
    units: FontUnits,
    line_height: f32,
}

/// What a style handle holds in memory, in its `Arc` (two counts and the handle), for the budget.
const HANDLE: u64 = (2 * size_of::<usize>() + size_of::<ComputedStyle>()) as u64;
/// What the values of a style hold in memory, in their `Arc`, for the budget.
const VALUES: u64 = (2 * size_of::<usize>() + size_of::<ComputedValues>()) as u64;

/// Like [`compute_styles_in`], with the fonts of the document, whose metrics font-relative
/// lengths (`ex`, `ch`, `cap`, `ic`, `lh`) refer to.
pub fn compute_styles_with(
    document: &Document,
    rules: &RuleSet,
    viewport: (f32, f32),
    sizes: &ContainerSizes,
    fonts: &dyn FontMetrics,
) -> StyledDocument {
    let mut styled = StyledDocument {
        styles: vec![None; document.len()],
        pseudo: FxHashMap::default(),
        hidden_strings: FxHashMap::default(),
        held: fagus_dom::budget::hold(
            fagus_dom::budget::Kind::Styles,
            (document.len() * size_of::<Option<Arc<ComputedStyle>>>()) as u64,
        ),
    };
    // The styles of the elements in one with `display: none`, computed only for the named
    // strings they set and kept while styles are computed (see `SharedStyles::classes`).
    let strings = sets_strings(document, rules);
    let mut hidden_styles: Vec<Arc<ComputedStyle>> = Vec::new();
    let mut hidden_held = fagus_dom::budget::hold(fagus_dom::budget::Kind::Styles, 0);
    let registry = Registry::new(rules.property_rules());
    let mut expansions = Expansions::new(rules);
    // Custom properties of `style` attributes that refer to their element pass on to children.
    expansions.element_dependent = expansions.element_dependent
        || document.descendants(NodeId::DOCUMENT).any(|node| {
            document
                .element(node)
                .and_then(|data| data.attribute("style"))
                .is_some_and(|css| css.contains("sibling-") || css.contains("attr("))
        });
    let mut hints = Hints::default();
    let mut shared = SharedStyles::default();
    let ancestors = Ancestors::new(document);
    let units = FontUnitsCache::new(fonts);
    let mut root = ComputedStyle::default();
    root.font_units = units.units(&root);
    let root = Arc::new(root);
    let mut root_font_size = RootFont {
        size: root.font_size,
        units: root.font_units,
        line_height: units.line_height(&root),
    };
    // Each element with its parent's style and the query containers around it.
    let mut stack: Vec<(NodeId, Arc<ComputedStyle>, Chain)> = document
        .child_elements(NodeId::DOCUMENT)
        .map(|element| (element, Arc::clone(&root), Arc::from([])))
        .collect();
    stack.reverse();
    let scroll = ScrollPseudos::of(rules);
    // The values charged to the budget, by their address: elements whose styles share values
    // hold them once. The styles of the elements live as long as `styled`, so no address is
    // reused while styles are computed.
    let mut charged: FxHashSet<usize> = FxHashSet::default();
    while let Some((element, parent, chain)) = stack.pop() {
        fagus_dom::budget::check_time();
        let pseudos = styled.pseudo.len();
        let is_root = document.document_element() == Some(element);
        let containers = Containers {
            chain: &chain,
            sizes,
            parent: &parent,
            registry: &registry,
        };
        let (style, class) = compute_element(
            document,
            rules,
            &containers,
            (&expansions, &registry, &units),
            (&mut hints, &mut shared),
            &ancestors,
            element,
            &parent,
            root_font_size,
            viewport,
            is_root,
        );
        let style = Arc::new(style);
        shared.classes.insert(Arc::as_ptr(&style) as usize, class);
        // Each element holds a handle of its own, and its values if no element before it has
        // the same.
        styled.held.add(HANDLE);
        if charged.insert(std::ptr::from_ref::<ComputedValues>(&style) as usize) {
            styled.held.add(VALUES);
        }
        if is_root {
            root_font_size = RootFont {
                size: style.font_size / style.zoom,
                units: style.font_units,
                line_height: units.line_height(&style) / style.zoom,
            };
            // Root-relative lengths refer to the root from now on.
            shared.built.clear();
        }
        // Pseudo-elements ask their originating element in unnamed style queries.
        let pseudo_containers = Containers {
            parent: &style,
            ..containers
        };
        for (key, kind) in [
            (PseudoKey::Before, PseudoElementKind::Before),
            (PseudoKey::After, PseudoElementKind::After),
        ] {
            if let Some(pseudo) = compute_pseudo(
                document,
                rules,
                &pseudo_containers,
                (&expansions, &registry, &units, &ancestors),
                element,
                kind,
                &[],
                &style,
                root_font_size,
                viewport,
            ) {
                let pseudo = ComputedStyle::from(ComputedValues {
                    before_of: (key == PseudoKey::Before).then_some(element),
                    // Only elements are footnotes, and only elements are resized: generated
                    // content gets no resize handle (CSS UI 4 leaves `resize` on it undefined;
                    // web-platform-tests expect none).
                    footnote: false,
                    resize: false,
                    ..ComputedValues::from(pseudo)
                });
                // A `::before` or `::after` that is a list item has a marker too.
                if matches!(pseudo.display, Display::Normal { list_item: true, .. })
                    && let Some(marker) = compute_pseudo_of(
                        document,
                        rules,
                        &pseudo_containers,
                        (&expansions, &registry, &units, &ancestors),
                        (element, PseudoElementKind::Marker, true),
                        &[],
                        &pseudo,
                        root_font_size,
                        viewport,
                    )
                {
                    let marker_key = if key == PseudoKey::Before {
                        PseudoKey::BeforeMarker
                    } else {
                        PseudoKey::AfterMarker
                    };
                    styled
                        .pseudo
                        .insert((element, marker_key), Arc::new(marker_style(marker, &pseudo)));
                }
                styled.pseudo.insert((element, key), Arc::new(pseudo));
            }
        }
        // The call and the marker of a footnote (CSS Generated Content for Paged Media 3, §2.4),
        // which the user agent stylesheet gives their content.
        if style.footnote {
            for (key, kind) in [
                (PseudoKey::FootnoteCall, PseudoElementKind::FootnoteCall),
                (PseudoKey::FootnoteMarker, PseudoElementKind::FootnoteMarker),
            ] {
                if let Some(pseudo) = compute_pseudo(
                    document,
                    rules,
                    &pseudo_containers,
                    (&expansions, &registry, &units, &ancestors),
                    element,
                    kind,
                    &[],
                    &style,
                    root_font_size,
                    viewport,
                ) {
                    let pseudo = ComputedStyle::from(ComputedValues {
                        footnote_marker: key == PseudoKey::FootnoteMarker,
                        ..ComputedValues::from(pseudo)
                    });
                    styled.pseudo.insert((element, key), Arc::new(pseudo));
                }
            }
        }
        // The first line and letter of a block container (CSS Pseudo-Elements 4, §2).
        if !matches!(style.display, Display::None | Display::Contents) && style.display != Display::INLINE {
            let first_line = compute_pseudo(
                document,
                rules,
                &pseudo_containers,
                (&expansions, &registry, &units, &ancestors),
                element,
                PseudoElementKind::FirstLine,
                &[],
                &style,
                root_font_size,
                viewport,
            )
            .map(Arc::new);
            let letter_parent = first_line.as_ref().unwrap_or(&style);
            if let Some(first_letter) = compute_pseudo(
                document,
                rules,
                &pseudo_containers,
                (&expansions, &registry, &units, &ancestors),
                element,
                PseudoElementKind::FirstLetter,
                &[],
                letter_parent,
                root_font_size,
                viewport,
            ) {
                styled
                    .pseudo
                    .insert((element, PseudoKey::FirstLetter), Arc::new(first_letter));
            }
            if let Some(first_line) = first_line {
                styled.pseudo.insert((element, PseudoKey::FirstLine), first_line);
            }
        }
        // The scroll marker group of a scroll container, the scroll markers and the scroll
        // buttons (CSS Overflow 5), which only author rules give the `content` they need.
        if style.display != Display::None {
            let mut wanted: Vec<(PseudoKey, PseudoElementKind)> = Vec::new();
            if scroll.marker {
                wanted.push((PseudoKey::ScrollMarker, PseudoElementKind::ScrollMarker));
            }
            if scroll.column_marker && style.is_multicol() {
                wanted.push((
                    PseudoKey::ColumnScrollMarker,
                    PseudoElementKind::ColumnScrollMarker,
                ));
            }
            for (&direction, &authored) in fagus_css::matching::ScrollDirection::ALL
                .iter()
                .zip(&scroll.buttons)
            {
                if authored {
                    wanted.push((
                        PseudoKey::ScrollButton(direction),
                        PseudoElementKind::ScrollButton(Some(direction)),
                    ));
                }
            }
            let group = style.scroll_marker_group != crate::values::ScrollMarkerGroup::None;
            if group {
                wanted.push((PseudoKey::ScrollMarkerGroup, PseudoElementKind::ScrollMarkerGroup));
            }
            for (key, kind) in wanted {
                let pseudo = compute_pseudo(
                    document,
                    rules,
                    &pseudo_containers,
                    (&expansions, &registry, &units, &ancestors),
                    element,
                    kind,
                    &[],
                    &style,
                    root_font_size,
                    viewport,
                );
                // A group is there without rules of its own too; markers and buttons only with
                // `content`.
                let pseudo = match pseudo {
                    None if key == PseudoKey::ScrollMarkerGroup => Some(style.inherit()),
                    Some(pseudo)
                        if key != PseudoKey::ScrollMarkerGroup
                            && matches!(
                                pseudo.content,
                                crate::values::Content::Normal | crate::values::Content::None
                            ) =>
                    {
                        None
                    }
                    pseudo => pseudo,
                };
                if let Some(pseudo) = pseudo {
                    styled.pseudo.insert((element, key), Arc::new(pseudo));
                }
            }
        }
        // The track and value of a progress bar, which it shows without its native look, and
        // those of a meter. The element sets the width of the value in its `style` attribute.
        let control = document.element(element).and_then(|data| {
            if data.is("progress") {
                let fraction = crate::controls::progress_fraction(data).unwrap_or(0.0);
                Some((
                    PseudoKey::ProgressBar,
                    PseudoElementKind::ProgressBar,
                    PseudoKey::ProgressValue,
                    PseudoElementKind::ProgressValue,
                    fraction,
                ))
            } else if data.is("meter") {
                let (fraction, region) = crate::controls::meter_value(data);
                Some((
                    PseudoKey::MeterBar,
                    PseudoElementKind::MeterBar,
                    PseudoKey::MeterValue,
                    region.pseudo_element(),
                    fraction,
                ))
            } else {
                None
            }
        });
        if let Some((bar_key, bar_kind, value_key, value_kind, fraction)) = control {
            // A meter with its native look takes the parts' native declarations, after the
            // other declarations of the user agent.
            let native = bar_key == PseudoKey::MeterBar
                && style.appearance == crate::values::Appearance::Auto
                && !style.author_frame;
            let user_agent = Precedence {
                band: CascadeBand::UserAgentNormal,
                layer: u32::MAX,
                specificity: u32::MAX,
                proximity: 0,
                order: u32::MAX,
            };
            let extra = |kind| -> Vec<(Precedence, &'static Property<'static>)> {
                if native {
                    crate::controls::native_meter_part(kind)
                        .iter()
                        .map(|property| (user_agent, property))
                        .collect()
                } else {
                    Vec::new()
                }
            };
            let bar = compute_pseudo(
                document,
                rules,
                &pseudo_containers,
                (&expansions, &registry, &units, &ancestors),
                element,
                bar_kind,
                &extra(bar_kind),
                &style,
                root_font_size,
                viewport,
            )
            .unwrap_or_else(|| style.inherit());
            let width = crate::controls::value_width(fraction);
            let mut value_extra: Vec<(Precedence, &Property<'static>)> = extra(value_kind);
            value_extra.push((
                Precedence {
                    band: CascadeBand::InlineNormal,
                    ..user_agent
                },
                &width,
            ));
            let value = compute_pseudo(
                document,
                rules,
                &pseudo_containers,
                (&expansions, &registry, &units, &ancestors),
                element,
                value_kind,
                &value_extra,
                &bar,
                root_font_size,
                viewport,
            )
            .unwrap_or_else(|| bar.inherit());
            styled.pseudo.insert((element, bar_key), Arc::new(bar));
            styled.pseudo.insert((element, value_key), Arc::new(value));
        }
        // The track and thumb of a slider, where rules style them.
        if document.element(element).is_some_and(|data| {
            data.is("input")
                && data
                    .attribute("type")
                    .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("range"))
        }) {
            let track = compute_pseudo(
                document,
                rules,
                &pseudo_containers,
                (&expansions, &registry, &units, &ancestors),
                element,
                PseudoElementKind::SliderTrack,
                &[],
                &style,
                root_font_size,
                viewport,
            )
            .map(Arc::new);
            let thumb = compute_pseudo(
                document,
                rules,
                &pseudo_containers,
                (&expansions, &registry, &units, &ancestors),
                element,
                PseudoElementKind::SliderThumb,
                &[],
                track.as_deref().unwrap_or(&style),
                root_font_size,
                viewport,
            );
            if let Some(track) = track {
                styled.pseudo.insert((element, PseudoKey::SliderTrack), track);
            }
            if let Some(thumb) = thumb {
                styled
                    .pseudo
                    .insert((element, PseudoKey::SliderThumb), Arc::new(thumb));
            }
        }
        // The placeholder text of a text field or text area that has one (`::placeholder`).
        if document.element(element).is_some_and(|data| {
            (data.is("input") || data.is("textarea")) && data.attribute("placeholder").is_some()
        }) && let Some(placeholder) = compute_pseudo(
            document,
            rules,
            &pseudo_containers,
            (&expansions, &registry, &units, &ancestors),
            element,
            PseudoElementKind::Placeholder,
            &[],
            &style,
            root_font_size,
            viewport,
        ) {
            styled
                .pseudo
                .insert((element, PseudoKey::Placeholder), Arc::new(placeholder));
        }
        if matches!(style.display, Display::Normal { list_item: true, .. }) {
            let marker = compute_pseudo(
                document,
                rules,
                &pseudo_containers,
                (&expansions, &registry, &units, &ancestors),
                element,
                PseudoElementKind::Marker,
                &[],
                &style,
                root_font_size,
                viewport,
            )
            .unwrap_or_else(|| style.inherit());
            styled.pseudo.insert(
                (element, PseudoKey::Marker),
                Arc::new(marker_style(marker, &style)),
            );
        }
        if style.display != Display::None {
            // A query container is one for its descendants, and so is a named element for
            // style queries.
            let chain: Chain = if style.container_type == crate::values::ContainerType::Normal
                && style.container_names.is_empty()
            {
                chain
            } else {
                chain
                    .iter()
                    .cloned()
                    .chain([(element, Arc::clone(&style))])
                    .collect()
            };
            let children: Vec<NodeId> = document.child_elements(element).collect();
            for child in children.into_iter().rev() {
                stack.push((child, Arc::clone(&style), Arc::clone(&chain)));
            }
        } else if strings {
            // The elements inside set their named strings where this one does (see
            // `StyledDocument::hidden_string_sets`): they get styles for that alone, without
            // pseudo-elements.
            let mut found = Vec::new();
            let mut inside: Vec<(NodeId, Arc<ComputedStyle>)> = document
                .child_elements(element)
                .map(|child| (child, Arc::clone(&style)))
                .collect();
            inside.reverse();
            while let Some((element, parent)) = inside.pop() {
                fagus_dom::budget::check_time();
                let containers = Containers {
                    chain: &chain,
                    sizes,
                    parent: &parent,
                    registry: &registry,
                };
                let (style, class) = compute_element(
                    document,
                    rules,
                    &containers,
                    (&expansions, &registry, &units),
                    (&mut hints, &mut shared),
                    &ancestors,
                    element,
                    &parent,
                    root_font_size,
                    viewport,
                    false,
                );
                let style = Arc::new(style);
                shared.classes.insert(Arc::as_ptr(&style) as usize, class);
                hidden_held.add(HANDLE + VALUES);
                if !style.string_set.is_empty() {
                    found.push((element, style.string_set.clone()));
                }
                let children: Vec<NodeId> = document.child_elements(element).collect();
                inside.extend(
                    children
                        .into_iter()
                        .rev()
                        .map(|child| (child, Arc::clone(&style))),
                );
                hidden_styles.push(style);
            }
            if !found.is_empty() {
                styled.hidden_strings.insert(element, found);
            }
        }
        // The styles of the element's pseudo-elements, each built for it.
        let pseudos = styled.pseudo.len() - pseudos;
        styled.held.add(pseudos as u64 * (HANDLE + VALUES));
        styled.styles[element.index()] = Some(style);
    }
    propagate_overflow(document, &mut styled);
    styled
}

/// Moves `overflow` from the root element, or else from the `body` element, to the viewport
/// (CSS Overflow 3, §3.3): the element it comes from keeps `visible`, so it neither clips its
/// content nor starts a block formatting context. Not the body's where it or the root element
/// has containment (CSS Contain 2, §3.2 and §3.3; Chrome 157 keeps it for any containment and
/// for query containers, measured). Chrome still paginates all content and
/// shrinks what is wider than the page to fit, as with Bulma's `html { overflow-x: hidden }`;
/// Fagus does not clip at the viewport.
fn propagate_overflow(document: &Document, styled: &mut StyledDocument) {
    use crate::values::Overflow;
    let Some(root) = document.document_element() else {
        return;
    };
    let clips = |styled: &StyledDocument, element: NodeId| {
        styled.styles[element.index()].as_ref().is_some_and(|style| {
            style.overflow_x != Overflow::Visible || style.overflow_y != Overflow::Visible
        })
    };
    let from = if clips(styled, root) {
        root
    } else {
        if !document.element(root).is_some_and(|data| data.is("html")) {
            return;
        }
        let Some(body) = document
            .child_elements(root)
            .find(|&child| document.element(child).is_some_and(|data| data.is("body")))
        else {
            return;
        };
        let contained = |element: NodeId| {
            styled.styles[element.index()]
                .as_ref()
                .is_some_and(|style| style.has_containment())
        };
        if !clips(styled, body) || contained(root) || contained(body) {
            return;
        }
        body
    };
    if let Some(style) = styled.styles[from.index()].as_mut() {
        let style = Arc::make_mut(style);
        style.overflow_x = Overflow::Visible;
        style.overflow_y = Overflow::Visible;
        style.scrollbar_x = crate::values::Scrollbar::None;
        style.scrollbar_y = crate::values::Scrollbar::None;
    }
}

/// Computes a style from declaration blocks applied in order, on top of `parent`. Used for
/// `@page` rules and page margin boxes, which inherit from the root element: only the longhands
/// that apply in a page context are kept, as Chrome keeps them.
pub fn style_from_declarations(
    blocks: &[&lightningcss::declaration::DeclarationBlock<'static>],
    parent: &ComputedValues,
    viewport: (f32, f32),
) -> ComputedStyle {
    let expansions = Expansions::default();
    let mut declarations = Declarations::default();
    for block in blocks {
        for property in &block.declarations {
            declarations.add(property, &expansions);
        }
    }
    for block in blocks {
        for property in &block.important_declarations {
            declarations.add(property, &expansions);
        }
    }
    declarations.retain(|id| crate::page_context::applies(id.name()));
    build_style(
        &declarations,
        (parent, None),
        RootFont {
            size: parent.font_size / parent.zoom,
            units: parent.font_units,
            line_height: parent.line_height_px(parent.font_size * 1.2) / parent.zoom,
        },
        (viewport, viewport),
        false,
        (&Registry::default(), &Functions::default()),
        &FontUnitsCache::new(&crate::values::NoFontMetrics),
    )
}

/// Styles computed for elements before, to share with elements whose style comes out the same
/// (siblings and cousins of the same kind: the rows of a table, their cells, the items of a
/// list). Styles are grouped in classes: two styles of one class are equal, as they were built
/// from the same declarations, in the same order, on parents of one class, and adjusted alike
/// for their elements. Each element still gets a style of its own (layout tells elements apart
/// by their styles), which shares the values of the built style unless an adjustment changes
/// them (see [`ComputedStyle`]).
#[derive(Default)]
struct SharedStyles {
    /// The class of each style computed so far, by its address (the styles stay alive while
    /// styles are computed, so addresses are not reused).
    classes: FxHashMap<usize, u32>,
    /// Styles as [`build_style`] gives them, with their class.
    built: FxHashMap<BuildKey, (ComputedStyle, u32)>,
    /// The class of a built style with the adjustments of an element (see [`Adjustments`]).
    adjusted: FxHashMap<(u32, Adjustments), u32>,
    next: u32,
}

/// What a built style depends on besides the root's font, which stays the same once the root's
/// style is known, and the viewport: the class of the parent's style, the declarations in
/// cascade order (by their addresses in the rule set, the presentational hints or the
/// expansions, which outlive the computation) and what container query units refer to.
#[derive(PartialEq, Eq, Hash)]
struct BuildKey {
    parent: u32,
    declarations: Vec<usize>,
    container: [u32; 2],
    /// The element's own `lang`, which font-relative lengths depend on.
    lang: Option<Box<str>>,
}

/// What [`compute_element`] changes in a built style for its element: the `lang` attribute,
/// whether author style sets the frame of a form control, the quirky margins, and whether the
/// element is an HTML table.
#[derive(PartialEq, Eq, Hash)]
struct Adjustments {
    lang: Option<Box<str>>,
    author_frame: bool,
    quirky_margins: (bool, bool),
    table: bool,
}

impl SharedStyles {
    fn fresh(&mut self) -> u32 {
        self.next += 1;
        self.next
    }

    /// The class of a style computed before (or a class of its own for one that was not).
    fn class_of(&mut self, style: &ComputedStyle) -> u32 {
        let address = std::ptr::from_ref(style) as usize;
        if let Some(class) = self.classes.get(&address) {
            return *class;
        }
        let class = self.fresh();
        self.classes.insert(address, class);
        class
    }

    /// The class of a built style of class `built` with `adjustments`.
    fn adjusted(&mut self, built: u32, adjustments: Adjustments) -> u32 {
        let next = self.next + 1;
        let class = *self.adjusted.entry((built, adjustments)).or_insert(next);
        if class == next {
            self.next = next;
        }
        class
    }
}

/// Computes the style of an element, with its class (see [`SharedStyles`]).
#[allow(clippy::too_many_arguments)]
fn compute_element(
    document: &Document,
    rules: &RuleSet,
    containers: &Containers<'_>,
    (expansions, registry, units): (&Expansions, &Registry, &FontUnitsCache<'_>),
    (hints, shared): (&mut Hints, &mut SharedStyles),
    ancestors: &Ancestors,
    element: NodeId,
    parent: &ComputedStyle,
    root_font_size: RootFont,
    viewport: (f32, f32),
    is_root: bool,
) -> (ComputedStyle, u32) {
    let mut matched = rules.matching_in(document, Some(ancestors), element, None);
    matched.retain(|rule| containers.admit(rule));
    let hints = hints.of(document, element);
    let inline = document
        .element(element)
        .and_then(|data| data.attribute("style"))
        .filter(|css| !fagus_css::nests_too_deep(css))
        .and_then(|css| {
            // Colors and lengths as Chrome reads them, in quirks mode or not.
            match fagus_css::author_css(css, document.quirks_mode()) {
                Some(css) => fagus_css::parse_declarations(&css),
                None => fagus_css::parse_declarations(css),
            }
        });

    let mut ordered: Vec<(Precedence, &Property<'static>)> = Vec::new();
    // Presentational hints are author declarations of specificity zero at the start of the author
    // style sheet (HTML, §15.2; CSS Cascade 4): first, at the lowest rank, so that any rule of the
    // same rank comes after them.
    if let Some(hints) = &hints {
        let precedence = Precedence {
            band: CascadeBand::AuthorNormal,
            layer: 0,
            specificity: 0,
            proximity: 0,
            order: 0,
        };
        ordered.extend(
            hints
                .declarations
                .declarations
                .iter()
                .map(|property| (precedence, property)),
        );
    }
    for rule in &matched {
        ordered.extend(
            rule.declarations
                .declarations
                .iter()
                .map(|property| (rule.precedence(false), property)),
        );
        ordered.extend(
            rule.declarations
                .important_declarations
                .iter()
                .map(|property| (rule.precedence(true), property)),
        );
    }
    if let Some(inline) = &inline {
        let precedence = |band, order| Precedence {
            band,
            layer: u32::MAX,
            specificity: u32::MAX,
            proximity: 0,
            order,
        };
        ordered.extend(
            inline
                .declarations
                .declarations
                .iter()
                .map(|property| (precedence(CascadeBand::InlineNormal, 0), property)),
        );
        ordered.extend(
            inline
                .declarations
                .important_declarations
                .iter()
                .map(|property| (precedence(CascadeBand::InlineImportant, 0), property)),
        );
    }
    ordered.sort_by_key(|(precedence, _)| *precedence);
    let author_frame = document.element(element).is_some_and(|data| {
        ["input", "textarea", "select", "button", "meter", "progress"]
            .iter()
            .any(|name| data.is(name))
    }) && author_sets_frame(&ordered);

    let quirky_margins = if document.quirks_mode() {
        quirky_margins(document, element, &ordered)
    } else {
        (false, false)
    };
    let basis = containers.basis(viewport);
    let lang = document
        .element(element)
        .and_then(fagus_dom::ElementData::language);
    // An inline style is parsed for its element alone: its declarations are not shared. Nor
    // are styles where values refer to the element itself (`attr()`, `sibling-index()`).
    let key = (inline.is_none() && !is_root && !expansions.element_dependent).then(|| BuildKey {
        parent: shared.class_of(parent),
        declarations: ordered
            .iter()
            .map(|(_, property)| std::ptr::from_ref(*property) as usize)
            .collect(),
        container: [basis.0.to_bits(), basis.1.to_bits()],
        lang: lang.map(Box::from),
    });
    let known = key.as_ref().and_then(|key| shared.built.get(key));
    let media = fagus_css::MediaEnvironment {
        width: viewport.0,
        height: viewport.1,
    };
    let functions = Functions {
        element: Some((document, element)),
        media: Some(&media),
        math: true,
        lengths: None,
    };
    let build = |ordered: &[(Precedence, &Property<'static>)]| {
        build_style(
            &cascade_declarations(ordered, expansions),
            (parent, lang),
            root_font_size,
            (viewport, basis),
            is_root,
            (registry, &functions),
            units,
        )
    };
    let (mut style, built) = if let Some((style, class)) = known {
        (style.clone(), Some(*class))
    } else {
        let style = build(&ordered);
        match key {
            Some(key) => {
                let class = shared.fresh();
                shared.built.insert(key, (style.clone(), class));
                (style, Some(class))
            }
            None => (style, None),
        }
    };
    animation::animate(
        &mut style,
        rules,
        &ordered,
        (expansions, registry, &functions),
        &build,
    );
    position_tries(&mut style, rules, &ordered, &build);
    // The adjustments below change a style only where they change a value, so that a style
    // shared with the elements of its class stays shared.
    if let Some(lang) = lang
        && style.lang.as_deref() != Some(lang)
    {
        style.lang = Some(lang.into());
    }
    // A shadow turns off the native look of a text field, a text area or a `<select>` too
    // (`LayoutTheme::IsControlStyled`).
    let author_frame = author_frame
        || (!style.box_shadow.is_empty() && document.element(element).is_some_and(is_text_control));
    if style.author_frame != author_frame {
        style.author_frame = author_frame;
    }
    if style.quirky_margins != quirky_margins {
        style.quirky_margins = quirky_margins;
    }
    // A `table` does not take `-webkit-left`, `-webkit-center` or `-webkit-right`: it is placed
    // by them, but its own text goes back to `start` (as Blink's style adjuster does).
    let table = document
        .element(element)
        .is_some_and(|data| data.is_html() && data.is("table"));
    if style.align_blocks && table {
        style.text_align = crate::values::TextAlign::Start;
        style.align_blocks = false;
    }
    let class = match built {
        Some(built) => shared.adjusted(
            built,
            Adjustments {
                lang: lang.map(Box::from),
                author_frame,
                quirky_margins,
                table,
            },
        ),
        None => shared.fresh(),
    };
    (style, class)
}

/// Declarations in cascade order, with their precedence.
type Ordered<'a> = [(Precedence, &'a Property<'static>)];

/// Applies the `@position-try` rules that an element's `position-try-fallbacks` name: each
/// option gets the element's style with the rule's declarations (of the properties such a rule
/// may set) cascaded above all author declarations but the important ones (CSS Anchor
/// Positioning 1, §5.1), built by `build` from declarations in cascade order.
fn position_tries(
    style: &mut ComputedStyle,
    rules: &RuleSet,
    ordered: &[(Precedence, &Property<'static>)],
    build: &dyn Fn(&Ordered<'_>) -> ComputedStyle,
) {
    use crate::anchor::TryOption;
    let Some(anchoring) = style.anchoring.as_ref().filter(|anchoring| {
        anchoring
            .fallbacks
            .iter()
            .any(|option| matches!(option, TryOption::Rule { .. }))
    }) else {
        return;
    };
    let mut anchoring = (**anchoring).clone();
    for option in &mut anchoring.fallbacks {
        let TryOption::Rule {
            name, style: tried, ..
        } = option
        else {
            continue;
        };
        let Some(block) = rules.position_try(name) else {
            continue;
        };
        let precedence = Precedence {
            band: CascadeBand::InlineNormal,
            layer: u32::MAX,
            specificity: u32::MAX,
            proximity: u32::MAX,
            order: u32::MAX,
        };
        let mut with: Vec<(Precedence, &Property<'static>)> = ordered.to_vec();
        with.extend(
            block
                .declarations
                .iter()
                .filter(|property| crate::anchor::applies_in_position_try(property.property_id().name()))
                .map(|property| (precedence, property)),
        );
        with.sort_by_key(|(precedence, _)| *precedence);
        let mut built = build(&with);
        // The rule's own fallbacks do not count.
        built.anchoring = built.anchoring.take().map(|own| {
            Arc::new(crate::anchor::Anchoring {
                fallbacks: Vec::new(),
                ..(*own).clone()
            })
        });
        *tried = Some(Arc::new(built));
    }
    style.anchoring = Some(Arc::new(anchoring));
}

/// Whether the top and bottom margins of an element in quirks mode are the quirky defaults of the
/// user agent stylesheet (see [`ComputedStyle::quirky_margins`]): those of the elements Blink
/// gives them, where no author declaration that comes later in the cascade sets them.
fn quirky_margins(
    document: &Document,
    element: NodeId,
    ordered: &[(Precedence, &Property<'static>)],
) -> (bool, bool) {
    let Some(data) = document.element(element).filter(|data| data.is_html()) else {
        return (false, false);
    };
    let name = &*data.name.local;
    let top = matches!(
        name,
        "p" | "blockquote"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ul"
            | "ol"
            | "menu"
            | "dir"
            | "dl"
            | "pre"
            | "xmp"
            | "plaintext"
            | "listing"
    );
    let bottom = matches!(name, "p" | "pre" | "xmp" | "plaintext" | "listing");
    if !top && !bottom {
        return (false, false);
    }
    // Which declarations come last for each side: those of the user agent keep the quirk.
    let (mut top_ua, mut bottom_ua) = (false, false);
    for (precedence, property) in ordered {
        let id = match property {
            Property::Unparsed(unparsed) => unparsed.property_id.clone(),
            _ => property.property_id(),
        };
        let user_agent = matches!(
            precedence.band,
            CascadeBand::UserAgentNormal | CascadeBand::UserAgentImportant
        );
        let all = matches!(property, Property::All(_));
        if all
            || matches!(
                id,
                PropertyId::Margin
                    | PropertyId::MarginTop
                    | PropertyId::MarginBlockStart
                    | PropertyId::MarginBlock
            )
        {
            top_ua = user_agent;
        }
        if all
            || matches!(
                id,
                PropertyId::Margin
                    | PropertyId::MarginBottom
                    | PropertyId::MarginBlockEnd
                    | PropertyId::MarginBlock
            )
        {
            bottom_ua = user_agent;
        }
    }
    (top && top_ua, bottom && bottom_ua)
}

/// Whether an element is a control whose native look a `box-shadow` turns off: a text field
/// (an `<input>` of a type that edits text), a `<textarea>` or a `<select>`.
fn is_text_control(data: &fagus_dom::ElementData) -> bool {
    if data.is("textarea") || data.is("select") {
        return true;
    }
    data.is("input")
        && !data.attribute("type").is_some_and(|kind| {
            matches!(
                kind.trim().to_ascii_lowercase().as_str(),
                "checkbox"
                    | "radio"
                    | "submit"
                    | "reset"
                    | "button"
                    | "image"
                    | "range"
                    | "color"
                    | "file"
                    | "hidden"
            )
        })
}

/// Whether author declarations decide a background or border property in the cascade (see
/// [`sets_frame`]): not where `revert` gives it back to the user agent, as Blink records the
/// origin of the value that wins (`LayoutTheme::IsControlStyled`).
fn author_sets_frame(ordered: &[(Precedence, &Property<'static>)]) -> bool {
    let mut winners: Vec<(PropertyId<'static>, bool)> = Vec::new();
    for (precedence, property) in ordered {
        let author = is_author(precedence) && rollback(property) != Some(Rollback::Origin);
        if let Property::All(_) = property {
            for winner in &mut winners {
                winner.1 = author;
            }
            continue;
        }
        if !sets_frame(property) {
            continue;
        }
        let id = match property {
            Property::Unparsed(unparsed) => unparsed.property_id.clone(),
            _ => property.property_id().into_owned(),
        };
        for id in longhand_ids(&id) {
            match winners.iter_mut().find(|(known, _)| *known == id) {
                Some(winner) => winner.1 = author,
                None => winners.push((id, author)),
            }
        }
    }
    winners.iter().any(|(_, author)| *author)
}

/// Whether a declaration sets a background or border property, `border-radius` included: what
/// in author style turns off the native look of a form control (Chrome 157 draws a text field
/// or a button with `border-radius: 0` with its CSS border, measured).
fn sets_frame(property: &Property<'static>) -> bool {
    let id = match property {
        Property::Unparsed(unparsed) => unparsed.property_id.clone(),
        _ => property.property_id(),
    };
    let name = id.name();
    (name.starts_with("background") || name.starts_with("border"))
        && !matches!(name, "border-collapse" | "border-spacing")
}

#[allow(clippy::too_many_arguments)]
fn compute_pseudo(
    document: &Document,
    rules: &RuleSet,
    containers: &Containers<'_>,
    (expansions, registry, units, ancestors): (&Expansions, &Registry, &FontUnitsCache<'_>, &Ancestors),
    element: NodeId,
    kind: PseudoElementKind,
    extra: &[(Precedence, &Property<'static>)],
    parent: &ComputedValues,
    root_font_size: RootFont,
    viewport: (f32, f32),
) -> Option<ComputedStyle> {
    compute_pseudo_of(
        document,
        rules,
        containers,
        (expansions, registry, units, ancestors),
        (element, kind, false),
        extra,
        parent,
        root_font_size,
        viewport,
    )
}

/// Which of the scroll pseudo-elements author rules style (CSS Overflow 5): only those need
/// their styles computed, since only such rules set `content`.
struct ScrollPseudos {
    marker: bool,
    column_marker: bool,
    /// For each of [`fagus_css::matching::ScrollDirection::ALL`].
    buttons: [bool; 4],
}

impl ScrollPseudos {
    fn of(rules: &RuleSet) -> Self {
        Self {
            marker: rules.authors_pseudo(PseudoElementKind::ScrollMarker),
            column_marker: rules.authors_pseudo(PseudoElementKind::ColumnScrollMarker),
            buttons: fagus_css::matching::ScrollDirection::ALL
                .map(|direction| rules.authors_pseudo(PseudoElementKind::ScrollButton(Some(direction)))),
        }
    }
}

/// The style of the marker of list item `item` as Blink's `AdjustStyleForMarker` leaves it: an
/// outside marker does not break and keeps its spaces; an inside one takes the item's
/// `white-space`.
fn marker_style(marker: ComputedStyle, item: &ComputedValues) -> ComputedStyle {
    if item.list_style_position == crate::values::ListStylePosition::Outside {
        ComputedStyle::from(ComputedValues {
            white_space: crate::values::WhiteSpace::PRE,
            ..ComputedValues::from(marker)
        })
    } else {
        marker
    }
}

/// [`compute_pseudo`], with only the rules of the user agent where `user_agent` (for the marker
/// of a `::before` or `::after`, which the rules for the element's own `::marker` do not style,
/// as in Chrome, whose `marker.css` names `::before::marker`).
#[allow(clippy::too_many_arguments)]
fn compute_pseudo_of(
    document: &Document,
    rules: &RuleSet,
    containers: &Containers<'_>,
    (expansions, registry, units, ancestors): (&Expansions, &Registry, &FontUnitsCache<'_>, &Ancestors),
    (element, kind, user_agent): (NodeId, PseudoElementKind, bool),
    extra: &[(Precedence, &Property<'static>)],
    parent: &ComputedValues,
    root_font_size: RootFont,
    viewport: (f32, f32),
) -> Option<ComputedStyle> {
    let mut matched = rules.matching_in(document, Some(ancestors), element, Some(kind));
    matched.retain(|rule| containers.admit(rule) && (!user_agent || rule.origin == RuleOrigin::UserAgent));
    if matched.is_empty()
        || (matches!(kind, PseudoElementKind::Before | PseudoElementKind::After)
            && !may_set_content(&matched))
    {
        return None;
    }
    let mut ordered: Vec<(Precedence, &Property<'static>)> = Vec::new();
    matched.sort_by_key(|rule| rule.order);
    for rule in &matched {
        ordered.extend(
            rule.declarations
                .declarations
                .iter()
                .map(|property| (rule.precedence(false), property)),
        );
        ordered.extend(
            rule.declarations
                .important_declarations
                .iter()
                .map(|property| (rule.precedence(true), property)),
        );
    }
    // Declarations of the user agent or of a `style` attribute that the pseudo-element has in
    // the shadow tree.
    ordered.extend_from_slice(extra);
    ordered.sort_by_key(|(precedence, _)| *precedence);
    let declarations = cascade_declarations(&ordered, expansions);
    let basis = containers.basis(viewport);
    let media = fagus_css::MediaEnvironment {
        width: viewport.0,
        height: viewport.1,
    };
    // `attr()` and `sibling-index()` refer to the originating element.
    let functions = Functions {
        element: Some((document, element)),
        media: Some(&media),
        math: true,
        lengths: None,
    };
    let mut style = build_style(
        &declarations,
        (parent, None),
        root_font_size,
        (viewport, basis),
        false,
        (registry, &functions),
        units,
    );
    let build = |ordered: &Ordered<'_>| {
        build_style(
            &cascade_declarations(ordered, expansions),
            (parent, None),
            root_font_size,
            (viewport, basis),
            false,
            (registry, &functions),
            units,
        )
    };
    animation::animate(
        &mut style,
        rules,
        &ordered,
        (expansions, registry, &functions),
        &build,
    );
    // `::before` and `::after` only generate a box if `content` is set.
    if matches!(kind, PseudoElementKind::Before | PseudoElementKind::After)
        && matches!(
            style.content,
            crate::values::Content::Normal | crate::values::Content::None
        )
    {
        return None;
    }
    // A scroll button has the native look of a button until the author styles its frame.
    if matches!(kind, PseudoElementKind::ScrollButton(_)) {
        style.author_frame = author_sets_frame(&ordered);
    }
    Some(style)
}

/// The longhands of a `flex-basis` or `flex` declaration whose basis is a sizing keyword
/// (`content`, `min-content`, `max-content`, `fit-content`, `stretch`), which lightningcss does
/// not parse: the basis stays as tokens, which computing it reads. `flex: <grow> <shrink>?
/// <basis>` takes a grow and shrink factor of 1 where they are left out.
fn flex_keyword_longhands(
    id: &PropertyId<'static>,
    tokens: &TokenList<'static>,
) -> Option<Vec<Property<'static>>> {
    const KEYWORDS: [&str; 7] = [
        "content",
        "min-content",
        "max-content",
        "fit-content",
        "stretch",
        "-webkit-fill-available",
        "-moz-available",
    ];
    let prefix = match id {
        PropertyId::FlexBasis(prefix) | PropertyId::Flex(prefix) => *prefix,
        _ => return None,
    };
    let mut keyword = None;
    let mut numbers = Vec::new();
    for token in &tokens.0 {
        match token {
            TokenOrValue::Token(Token::WhiteSpace(_)) => {}
            TokenOrValue::Token(Token::Ident(ident))
                if keyword.is_none() && KEYWORDS.iter().any(|known| ident.eq_ignore_ascii_case(known)) =>
            {
                keyword = Some(token.clone());
            }
            TokenOrValue::Token(Token::Number { value, .. })
                if keyword.is_none() && numbers.len() < 2 && matches!(id, PropertyId::Flex(_)) =>
            {
                numbers.push(*value);
            }
            _ => return None,
        }
    }
    let basis = Property::Unparsed(UnparsedProperty {
        property_id: PropertyId::FlexBasis(prefix),
        value: TokenList(vec![keyword?]),
    });
    if matches!(id, PropertyId::FlexBasis(_)) {
        return Some(vec![basis]);
    }
    let grow = numbers.first().copied().unwrap_or(1.0);
    let shrink = numbers.get(1).copied().unwrap_or(1.0);
    Some(vec![
        Property::FlexGrow(grow, prefix),
        Property::FlexShrink(shrink, prefix),
        basis,
    ])
}

/// The value a declaration gives a property: borrowed where it is used as declared, made where
/// `var()` is substituted. `None` if it is invalid at computed-value time.
fn resolve<'d, 'v>(
    declared: &'d Declared<'_>,
    id: &PropertyId<'static>,
    lookup: &impl Fn(&str) -> Option<&'v TokenList<'static>>,
    functions: &Functions<'_>,
) -> Option<Result<std::borrow::Cow<'d, Property<'static>>, Keyword>> {
    let property: &Property<'static> = match declared {
        Declared::Value(property) => property,
        Declared::Owned(property) => property,
        Declared::Keyword(keyword) => return Some(Err(*keyword)),
        Declared::Variable(unparsed) => {
            // Invalid at computed-value time: behaves like `unset`.
            let tokens = variables::substitute_with(&unparsed.value, lookup, functions)?;
            // `position: running(name)`, `subgrid` and `flex-wrap: balance` stay unparsed;
            // computing them sets `running`, the track list and `flex_balance`.
            if (unparsed.property_id == PropertyId::Position
                && fagus_css::paged::running_name(&tokens).is_some())
                || (matches!(unparsed.property_id, PropertyId::FlexWrap(_))
                    && fagus_css::flex_wrap_balance(&tokens).is_some())
                || (matches!(
                    unparsed.property_id,
                    PropertyId::GridTemplateColumns | PropertyId::GridTemplateRows
                ) && fagus_css::paged::subgrid_line_names(&tokens).is_some())
            {
                return Some(Ok(std::borrow::Cow::Owned(Property::Unparsed(
                    UnparsedProperty {
                        property_id: unparsed.property_id.clone(),
                        value: tokens,
                    },
                ))));
            }
            // Anchor positioning: the longhand's tokens stay for layout.
            let anchored = UnparsedProperty {
                property_id: unparsed.property_id.clone(),
                value: tokens,
            };
            if fagus_css::is_anchored(&anchored) || fagus_css::is_calc_size(&anchored) {
                let tokens = anchored.value;
                let longhands = longhand_ids(&unparsed.property_id);
                let value = match longhands.iter().position(|longhand| longhand == id) {
                    Some(_) if longhands.len() == 1 => tokens,
                    Some(index) => crate::anchor::longhand_value(&tokens, index, longhands.len())?,
                    None => return Some(Err(Keyword::Unset)),
                };
                return Some(Ok(std::borrow::Cow::Owned(Property::Unparsed(
                    UnparsedProperty {
                        property_id: id.clone(),
                        value,
                    },
                ))));
            }
            let tokens = anchored.value;
            // `grid` and `grid-template` with `subgrid`, which lightningcss does not parse.
            if matches!(unparsed.property_id, PropertyId::Grid | PropertyId::GridTemplate)
                && let Some(longhand) = subgrid_shorthand(&tokens, id)
            {
                return Some(longhand.map(std::borrow::Cow::Owned));
            }
            // `grid-auto-flow: dense`, which lightningcss rejects without `row` or `column`
            // after it: `row dense`.
            if unparsed.property_id == PropertyId::GridAutoFlow && fagus_css::is_dense(&tokens) {
                use lightningcss::properties::grid::GridAutoFlow;
                return Some(Ok(std::borrow::Cow::Owned(Property::GridAutoFlow(
                    GridAutoFlow::Row | GridAutoFlow::Dense,
                ))));
            }
            // `flex-basis` and `flex` with a sizing keyword, which lightningcss does not parse.
            if let Some(longhands) = flex_keyword_longhands(&unparsed.property_id, &tokens) {
                return Some(
                    longhands
                        .into_iter()
                        .find(|longhand| cascade_id(longhand) == *id)
                        .map(std::borrow::Cow::Owned)
                        .ok_or(Keyword::Unset),
                );
            }
            let longhands = if matches!(
                unparsed.property_id,
                PropertyId::Background | PropertyId::BackgroundImage
            ) && fagus_css::gradient::needs_repair(&tokens)
            {
                background::repaired_longhands(&unparsed.property_id, &tokens)?
            } else {
                expand(&variables::parse_property(&unparsed.property_id, &tokens)?)
            };
            return Some(
                longhands
                    .into_iter()
                    .find(|longhand| cascade_id(longhand) == *id)
                    .map(std::borrow::Cow::Owned)
                    .ok_or(Keyword::Unset),
            );
        }
    };
    match property {
        // Properties lightningcss does not know keep their tokens, `var()` included.
        Property::Custom(custom) if variables::has_variables(&custom.value) => {
            let value = variables::substitute_with(&custom.value, lookup, functions)?;
            Some(Ok(std::borrow::Cow::Owned(Property::Custom(CustomProperty {
                name: custom.name.clone(),
                value,
            }))))
        }
        property => Some(Ok(std::borrow::Cow::Borrowed(property))),
    }
}

/// Longhand `id` of `grid` or `grid-template` given `rows / columns` where either is
/// `subgrid` (CSS Grid 2, §7.4 and §7.8): the track lists, as unparsed `subgrid` values or
/// parsed track lists, and the initial values of the other longhands. `None` where the value
/// is not of that form; an invalid side makes the declaration invalid (`unset`).
fn subgrid_shorthand(
    tokens: &TokenList<'static>,
    id: &PropertyId<'static>,
) -> Option<Result<Property<'static>, Keyword>> {
    let (rows, columns) = fagus_css::paged::subgrid_shorthand_sides(tokens)?;
    let subgrid = |side: &TokenList<'static>| fagus_css::paged::subgrid_line_names(side).is_some();
    let side = match id {
        PropertyId::GridTemplateRows => rows,
        PropertyId::GridTemplateColumns => columns,
        _ => return Some(Err(Keyword::Initial)),
    };
    if subgrid(&side) {
        return Some(Ok(Property::Unparsed(UnparsedProperty {
            property_id: id.clone(),
            value: side,
        })));
    }
    Some(variables::parse_property(id, &side).ok_or(Keyword::Unset))
}

fn build_style(
    declarations: &Declarations<'_>,
    (parent, lang): (&ComputedValues, Option<&str>),
    root: RootFont,
    (viewport, container): ((f32, f32), (f32, f32)),
    is_root: bool,
    (registry, functions): (&Registry, &Functions<'_>),
    units: &FontUnitsCache<'_>,
) -> ComputedStyle {
    let mut style = parent.inherit();
    // The element's own language picks the fonts its font-relative lengths refer to.
    if let Some(lang) = lang {
        style.lang = Some(lang.into());
    }
    let initial = ComputedStyle::initial_values();

    // Registered custom properties that do not inherit start from their initial value.
    if !registry.is_empty()
        && registry
            .non_inherited()
            .any(|name| parent.custom_properties.contains_key(name))
    {
        let mut custom = (*parent.custom_properties).clone();
        for name in registry.non_inherited() {
            custom.remove(name);
        }
        style.custom_properties = Arc::new(custom);
    }
    // Custom properties first: other declarations may refer to them. Their computed values have
    // all references substituted, so children look values up without substituting again.
    if !declarations.custom.is_empty() && !restates_custom(&declarations.custom, &style.custom_properties) {
        let mut custom = (*style.custom_properties).clone();
        for (name, value) in &declarations.custom {
            // A CSS-wide keyword alone: `initial` is the guaranteed-invalid value (or the
            // registered initial value, looked up later), `inherit` the parent's value, and
            // `unset` either of them, as the property inherits or not (CSS Variables 1, §2).
            let inherits = match keyword(value) {
                None => {
                    custom.insert(Arc::from(name.as_str()), Arc::new((*value).clone()));
                    continue;
                }
                Some(Keyword::Inherit) => true,
                Some(Keyword::Initial) => false,
                Some(Keyword::Unset) => registry.inherits(name),
            };
            match parent.custom_properties.get(name.as_str()).filter(|_| inherits) {
                Some(inherited) => custom.insert(Arc::from(name.as_str()), Arc::clone(inherited)),
                None => custom.remove(name.as_str()),
            };
        }
        for (name, _) in &declarations.custom {
            let substituted = {
                let Some(raw) = custom.get(name.as_str()) else {
                    continue;
                };
                if variables::has_variables(raw) {
                    let lookup = |reference: &str| {
                        custom
                            .get(reference)
                            .map(AsRef::as_ref)
                            .or_else(|| registry.initial(reference))
                    };
                    let functions = Functions {
                        math: false,
                        ..*functions
                    };
                    variables::substitute_with(raw, &lookup, &functions)
                } else if registry.has_syntax(name) {
                    Some((**raw).clone())
                } else {
                    continue;
                }
            };
            match substituted {
                Some(value) if registry.accepts(name, &value) => {
                    custom.insert(Arc::from(name.as_str()), Arc::new(value));
                }
                // A registered property with a value not of its syntax is unset.
                Some(_) => {
                    match parent
                        .custom_properties
                        .get(name.as_str())
                        .filter(|_| registry.inherits(name))
                    {
                        Some(inherited) => custom.insert(Arc::from(name.as_str()), Arc::clone(inherited)),
                        None => custom.remove(name.as_str()),
                    };
                }
                // Invalid at computed-value time: the property has no value.
                None => {
                    custom.remove(name.as_str());
                }
            }
        }
        style.custom_properties = Arc::new(custom);
    }
    let custom_properties = Arc::clone(&style.custom_properties);
    let lookup = |name: &str| {
        custom_properties
            .get(name)
            .map(AsRef::as_ref)
            .or_else(|| registry.initial(name))
    };

    // Math functions that Fagus evaluates resolve relative lengths as the property does.
    let resolve = |declared, id, context: Option<&Context>| match context {
        Some(context) => {
            let lengths = |value: &LengthValue| Some(context.length_value(value) / context.zoom);
            let functions = Functions {
                lengths: Some(&lengths),
                ..*functions
            };
            resolve(declared, id, &lookup, &functions)
        }
        None => resolve(declared, id, &lookup, functions),
    };

    // `zoom` first: the absolute lengths of all other properties are multiplied by the
    // effective zoom, and inherited lengths are scaled to it.
    let zoom_id = zoom::zoom_id();
    if let Some(&index) = declarations.index.get(&zoom_id)
        && let Some((id, declared)) = &declarations.entries[index]
    {
        let own = match resolve(declared, id, None) {
            Some(Ok(property)) => match &*property {
                Property::Custom(custom) => zoom::specified(&custom.value),
                _ => None,
            },
            // `zoom` does not inherit: `inherit` takes the parent's own zoom (Chrome 157,
            // measured), the other keywords its initial value.
            Some(Err(Keyword::Inherit)) => Some(parent.specified_zoom),
            Some(Err(_)) | None => Some(1.0),
        };
        if let Some(own) = own {
            style.specified_zoom = own;
            style.zoom = parent.zoom * own;
        }
    }
    let ratio = style.zoom / parent.zoom;
    if (ratio - 1.0).abs() > 1e-6 {
        zoom::rezoom_inherited(&mut style, ratio);
    }
    // The initial widths of borders, outlines and column rules are `medium`, 3px zoomed. Styles
    // that are not computed from declarations (anonymous boxes) keep borders of no width.
    let medium = compute::snap_border_width(3.0 * style.zoom);
    style.border_width = crate::values::Sides::all(medium);
    if (style.zoom - 1.0).abs() > 1e-6 {
        style.outline_width = medium;
        style.column_rule_width = medium;
    }

    // The lengths of the root element that refer to the root refer to the initial values.
    let parent_line_height = units.line_height(parent);
    let mut context = Context {
        font_size: style.font_size,
        parent_font_size: style.font_size,
        parent_font_size_absolute: parent.font_size_basis.absolute,
        root_font_size: if is_root {
            parent.font_size / parent.zoom
        } else {
            root.size
        },
        units: parent.font_units,
        root_units: if is_root { parent.font_units } else { root.units },
        line_height: parent_line_height * ratio,
        root_line_height: if is_root {
            parent_line_height / parent.zoom
        } else {
            root.line_height
        },
        zoom: style.zoom,
        viewport,
        container,
        rtl: parent.is_rtl(),
        writing_mode: parent.writing_mode,
        upright: sets_upright(parent),
        parent_rtl: parent.is_rtl(),
        root: is_root,
    };

    // Whether the family is `monospace` alone, whose text Chrome sizes from its fixed font size.
    let mut monospace = parent.font_size_basis.monospace;
    if let Some(&index) = declarations.index.get(&PropertyId::FontFamily)
        && let Some((id, declared)) = &declarations.entries[index]
    {
        match resolve(declared, id, Some(&context)) {
            Some(Ok(property)) => {
                if let Property::FontFamily(families) = &*property {
                    monospace = compute::is_monospace(families);
                }
            }
            Some(Err(Keyword::Initial)) => monospace = false,
            _ => {}
        }
    }
    style.font_size_basis.monospace = monospace;

    // `math-style`, `math-shift` and `math-depth` before the font size, which `font-size: math`
    // scales by the depth.
    for name in ["math-style", "math-shift", "math-depth"] {
        let id = PropertyId::Custom(CustomPropertyName::Unknown(lightningcss::values::ident::Ident(
            name.into(),
        )));
        let Some(&index) = declarations.index.get(&id) else {
            continue;
        };
        let Some((id, declared)) = &declarations.entries[index] else {
            continue;
        };
        match resolve(declared, id, None) {
            Some(Ok(property)) => {
                if let Property::Custom(custom) = &*property {
                    match name {
                        "math-style" => {
                            style.math_compact =
                                crate::mathml::compact(&custom.value).unwrap_or(style.math_compact);
                        }
                        "math-shift" => {
                            style.math_shift_compact =
                                crate::mathml::compact(&custom.value).unwrap_or(style.math_shift_compact);
                        }
                        _ => {
                            style.math_depth =
                                crate::mathml::depth(&custom.value, parent).unwrap_or(style.math_depth);
                        }
                    }
                }
            }
            Some(Err(Keyword::Initial)) => match name {
                "math-style" => style.math_compact = false,
                "math-shift" => style.math_shift_compact = false,
                _ => style.math_depth = 0,
            },
            _ => {}
        }
    }

    // `font-size` first: `em` in all other properties refers to it.
    if let Some(&index) = declarations.index.get(&PropertyId::FontSize)
        && let Some((id, declared)) = &declarations.entries[index]
    {
        // `font-size: math` scales the parent's size by the math depth, as the parent's first
        // available font's `MATH` table says.
        let math = matches!(declared, Declared::Variable(unparsed)
            if crate::mathml::is_math_font_size(&unparsed.value));
        let resolved = if math {
            None
        } else {
            resolve(declared, id, Some(&context))
        };
        if math {
            style.font_size = crate::mathml::scaled_font_size(
                parent.font_size,
                (parent.math_depth, style.math_depth),
                parent.font_units.math_scale,
            );
        }
        match resolved {
            Some(Ok(property)) => {
                if let Property::FontSize(size) = &*property {
                    (style.font_size, style.font_size_basis) = compute::font_size(&context, size, monospace);
                }
            }
            Some(Err(Keyword::Initial)) => {
                style.font_size_basis = FontSizeBasis {
                    monospace,
                    ..FontSizeBasis::default()
                };
                style.font_size = compute::keyword_font_size(3, monospace) * style.zoom;
            }
            _ => {}
        }
    }
    // A size that does not come from an absolute length follows the family to or from
    // `monospace`: a keyword is looked up again, another size scaled by 13/16.
    let basis = style.font_size_basis;
    if basis.monospace != parent.font_size_basis.monospace && !basis.absolute {
        style.font_size = match basis.keyword {
            Some(keyword) => compute::keyword_font_size(keyword, monospace) * style.zoom,
            None if monospace => style.font_size * 13.0 / 16.0,
            None => style.font_size * 16.0 / 13.0,
        };
    }
    context.font_size = style.font_size;
    if is_root {
        context.root_font_size = style.font_size / style.zoom;
    }

    // `color-scheme` next: the colors of all other properties follow it.
    if let Some(&index) = declarations.index.get(&PropertyId::ColorScheme)
        && let Some((id, declared)) = &declarations.entries[index]
    {
        match resolve(declared, id, Some(&context)) {
            Some(Ok(property)) => compute::apply(&property, &mut style, parent, &context),
            Some(Err(keyword)) => apply_keyword(id, keyword, &mut style, parent, initial),
            None => apply_keyword(id, Keyword::Unset, &mut style, parent, initial),
        }
    }
    compute::set_dark(style.dark);

    // `color` next: `currentColor` in other properties refers to it. Its initial value is
    // `CanvasText`, white in the dark color scheme.
    compute::set_current_color(Some(parent.color));
    if let Some(&index) = declarations.index.get(&PropertyId::Color)
        && let Some((id, declared)) = &declarations.entries[index]
    {
        match resolve(declared, id, Some(&context)) {
            Some(Ok(property)) => compute::apply(&property, &mut style, parent, &context),
            Some(Err(Keyword::Initial)) if style.dark => style.color = crate::values::Rgba::WHITE,
            Some(Err(Keyword::Initial)) => style.color = initial.color,
            _ => {}
        }
    }
    compute::set_current_color(Some(style.color));

    // `direction` next: logical properties map to the sides it gives them.
    if let Some(&index) = declarations.index.get(&PropertyId::Direction)
        && let Some((id, declared)) = &declarations.entries[index]
    {
        match resolve(declared, id, Some(&context)) {
            Some(Ok(property)) => compute::apply(&property, &mut style, parent, &context),
            Some(Err(Keyword::Initial)) => style.direction = initial.direction,
            _ => {}
        }
    }
    context.rtl = style.is_rtl();
    // `writing-mode` too.
    let writing_mode = writing_mode_id();
    if let Some(&index) = declarations.index.get(&writing_mode)
        && let Some((id, declared)) = &declarations.entries[index]
    {
        match resolve(declared, id, Some(&context)) {
            Some(Ok(property)) => compute::apply(&property, &mut style, parent, &context),
            Some(Err(keyword)) => apply_keyword(id, keyword, &mut style, parent, initial),
            None => apply_keyword(id, Keyword::Unset, &mut style, parent, initial),
        }
    }
    context.writing_mode = style.writing_mode;
    // `text-orientation` too: `ch` refers to the vertical advance of the "0" in upright text.
    let text_orientation = text_orientation_id();
    if let Some(&index) = declarations.index.get(&text_orientation)
        && let Some((id, declared)) = &declarations.entries[index]
    {
        match resolve(declared, id, Some(&context)) {
            Some(Ok(property)) => compute::apply(&property, &mut style, parent, &context),
            Some(Err(keyword)) => apply_keyword(id, keyword, &mut style, parent, initial),
            None => apply_keyword(id, Keyword::Unset, &mut style, parent, initial),
        }
    }
    context.upright = sets_upright(&style);

    // The font properties next: `ex`, `ch`, `cap` and `ic` refer to the first available font
    // they select.
    for id in FONT_SELECTION {
        if let Some(&index) = declarations.index.get(&id)
            && let Some((id, declared)) = &declarations.entries[index]
        {
            match resolve(declared, id, Some(&context)) {
                Some(Ok(property)) => compute::apply(&property, &mut style, parent, &context),
                Some(Err(keyword)) => apply_keyword(id, keyword, &mut style, parent, initial),
                None => apply_keyword(id, Keyword::Unset, &mut style, parent, initial),
            }
        }
    }
    if !(Arc::ptr_eq(&style.font_family, &parent.font_family)
        && style.font_weight.to_bits() == parent.font_weight.to_bits()
        && style.font_style == parent.font_style
        && style.font_stretch.to_bits() == parent.font_stretch.to_bits()
        && style.lang == parent.lang)
    {
        style.font_units = units.units(&style);
    }
    // `font-size-adjust` next: `ex`, `ch`, `cap` and `ic` refer to the font as it scales it.
    let font_size_adjust = font_size_adjust_id();
    if let Some(&index) = declarations.index.get(&font_size_adjust)
        && let Some((id, declared)) = &declarations.entries[index]
    {
        match resolve(declared, id, Some(&context)) {
            Some(Ok(property)) => compute::apply(&property, &mut style, parent, &context),
            Some(Err(keyword)) => apply_keyword(id, keyword, &mut style, parent, initial),
            None => apply_keyword(id, Keyword::Unset, &mut style, parent, initial),
        }
    }
    context.units = units.adjusted_units(&style);
    if is_root {
        context.root_units = context.units;
    }

    // `line-height` next (its `lh` the parent's): `lh` in other properties refers to it.
    if let Some(&index) = declarations.index.get(&PropertyId::LineHeight)
        && let Some((id, declared)) = &declarations.entries[index]
    {
        match resolve(declared, id, Some(&context)) {
            Some(Ok(property)) => compute::apply(&property, &mut style, parent, &context),
            Some(Err(keyword)) => apply_keyword(id, keyword, &mut style, parent, initial),
            None => apply_keyword(id, Keyword::Unset, &mut style, parent, initial),
        }
    }
    context.line_height = units.line_height(&style);
    if is_root {
        context.root_line_height = context.line_height / style.zoom;
    }

    let mut svg_paint = Vec::new();
    for (id, declared) in declarations.entries.iter().flatten() {
        if matches!(
            id,
            PropertyId::FontSize
                | PropertyId::Color
                | PropertyId::Direction
                | PropertyId::FontFamily
                | PropertyId::FontWeight
                | PropertyId::FontStyle
                | PropertyId::FontStretch
                | PropertyId::LineHeight
        ) || *id == writing_mode
            || *id == text_orientation
            || *id == font_size_adjust
            || *id == zoom_id
        {
            continue;
        }
        if *id == PropertyId::BackgroundColor {
            style.background_color_current = current_background(declared, &lookup, functions);
        }
        match resolve(declared, id, Some(&context)) {
            Some(Ok(property)) => match compute::svg_paint(&property) {
                Some(declaration) => svg_paint.push(declaration),
                None => compute::apply(&property, &mut style, parent, &context),
            },
            Some(Err(keyword)) => apply_keyword(id, keyword, &mut style, parent, initial),
            None => apply_keyword(id, Keyword::Unset, &mut style, parent, initial),
        }
    }
    if !svg_paint.is_empty() {
        style.svg_paint = Arc::from(svg_paint);
    }

    finish(&mut style, is_root);
    if !is_root {
        // `writing-mode` does not apply to table rows, row groups, columns and column groups:
        // they take their parent's, after their lengths are computed in their own, and their
        // children inherit it (Blink's `StyleAdjuster`).
        if matches!(
            style.display,
            Display::Internal(
                crate::values::InternalDisplay::TableRow
                    | crate::values::InternalDisplay::TableRowGroup
                    | crate::values::InternalDisplay::TableHeaderGroup
                    | crate::values::InternalDisplay::TableFooterGroup
                    | crate::values::InternalDisplay::TableColumn
                    | crate::values::InternalDisplay::TableColumnGroup
            )
        ) {
            style.writing_mode = parent.writing_mode;
        }
        crate::webkit_box::finish_item(&mut style, parent);
        // `z-index` applies to flex and grid items without a position too: such an item with
        // one is a stacking context (CSS Flexbox 1, §4.3; CSS Grid 2, §9.2), an isolated group
        // as every stacking context is, which `isolation` makes it.
        if style.z_index.is_some()
            && !style.is_positioned()
            && matches!(
                parent.display,
                Display::Normal {
                    inside: crate::values::DisplayInside::Flex | crate::values::DisplayInside::Grid,
                    ..
                }
            )
        {
            style.isolation = crate::values::Isolation::Isolate;
        }
    }
    if !is_root && style.writing_mode != parent.writing_mode {
        style.display = flow_root_display(style.display);
    }
    // `aspect-ratio` does not apply to the internal boxes of tables (CSS Box Sizing 4, §5.1),
    // which Chrome sizes as if they had none.
    if matches!(style.display, Display::Internal(internal) if internal != crate::values::InternalDisplay::TableCaption)
    {
        style.aspect_ratio = None;
    }
    if !is_root {
        stretch_subgrid(&mut style, parent);
    }
    style.decorations = decorations(&style, parent);
    ComputedStyle::from(style)
}

/// The value of a declared `background-color` that depends on `currentcolor`, kept for the
/// elements that inherit it (with `var()` substituted, the color functions not resolved).
fn current_background<'v>(
    declared: &Declared<'_>,
    lookup: &impl Fn(&str) -> Option<&'v TokenList<'static>>,
    functions: &Functions<'_>,
) -> Option<Arc<TokenList<'static>>> {
    match declared {
        Declared::Value(Property::BackgroundColor(CssColor::CurrentColor)) => Some(Arc::new(TokenList(
            vec![TokenOrValue::Color(CssColor::CurrentColor)],
        ))),
        Declared::Owned(property)
            if matches!(**property, Property::BackgroundColor(CssColor::CurrentColor)) =>
        {
            Some(Arc::new(TokenList(vec![TokenOrValue::Color(
                CssColor::CurrentColor,
            )])))
        }
        Declared::Variable(unparsed)
            if matches!(
                unparsed.property_id,
                PropertyId::BackgroundColor | PropertyId::Background
            ) =>
        {
            let functions = Functions {
                math: false,
                ..*functions
            };
            let tokens = variables::substitute_with(&unparsed.value, lookup, &functions)?;
            crate::current_color::background_color(tokens, unparsed.property_id == PropertyId::Background)
        }
        _ => None,
    }
}

/// A subgrid fills the tracks it spans along its subgridded axes: its sizes there and its
/// self-alignment across them are ignored (CSS Grid 2, §9). A grid that is absolutely
/// positioned or contains its layout is an independent formatting context and no subgrid.
fn stretch_subgrid(style: &mut ComputedValues, parent: &ComputedValues) {
    use crate::values::{ContainerType, DisplayInside, ItemAlignment, MaxSize, Size};
    let grid = |display: Display| {
        matches!(
            display,
            Display::Normal {
                inside: DisplayInside::Grid,
                ..
            }
        )
    };
    if !grid(style.display)
        || !grid(parent.display)
        || style.is_out_of_flow()
        || style.contain_layout
        || style.container_type != ContainerType::Normal
    {
        return;
    }
    // The physical dimension of each subgridded axis, true for the width: columns run along
    // the lines, which are vertical in a vertical writing mode.
    let vertical = style.writing_mode.is_vertical();
    let axes = [
        (style.grid_template_columns.subgrid, !vertical),
        (style.grid_template_rows.subgrid, vertical),
    ];
    // The parent's inline axis, which `justify-self` aligns along.
    let parent_inline_width = !parent.writing_mode.is_vertical();
    for (subgrid, width) in axes {
        if !subgrid {
            continue;
        }
        if width {
            style.width = Size::Auto;
            style.min_width = Size::Auto;
            style.max_width = MaxSize::None;
        } else {
            style.height = Size::Auto;
            style.min_height = Size::Auto;
            style.max_height = MaxSize::None;
        }
        if width == parent_inline_width {
            style.justify_self = ItemAlignment::Stretch;
        } else {
            style.align_self = ItemAlignment::Stretch;
        }
    }
}

/// Whether declared custom properties only restate the values an element has already (as
/// `*, ::before, ::after { --tw-…: … }` of Tailwind does on every element): plain values, without
/// references or CSS-wide keywords, equal to those inherited. Setting them would give the same
/// properties, so the inherited ones are shared instead of copied.
fn restates_custom(
    declared: &[(String, &TokenList<'static>)],
    inherited: &crate::style::CustomProperties,
) -> bool {
    declared.iter().all(|(name, value)| {
        keyword(value).is_none()
            && !variables::has_variables(value)
            && inherited
                .get(name.as_str())
                .is_some_and(|known| **known == **value)
    })
}

/// Whether a `::before` or `::after` with these rules may generate a box: whether one of them
/// sets `content` (or `all`). Without, `content` keeps its initial value, `normal`, which
/// generates no box.
fn may_set_content(rules: &[fagus_css::MatchedRule<'_>]) -> bool {
    rules.iter().any(|rule| {
        rule.declarations
            .declarations
            .iter()
            .chain(&rule.declarations.important_declarations)
            .any(|property| match property {
                Property::All(_) => true,
                Property::Custom(CustomProperty {
                    name: CustomPropertyName::Unknown(name),
                    ..
                }) => name.0.eq_ignore_ascii_case("content"),
                _ => false,
            })
    })
}

/// The properties that select an element's fonts.
const FONT_SELECTION: [PropertyId<'static>; 4] = [
    PropertyId::FontFamily,
    PropertyId::FontWeight,
    PropertyId::FontStyle,
    PropertyId::FontStretch,
];

/// The cascade's key of `writing-mode`, which lightningcss keeps as tokens.
/// Whether a style sets its text upright in vertical lines, where `ch` refers to the vertical
/// advance of the "0" (Blink's `IsVerticalUpright` of the "0").
fn sets_upright(style: &ComputedValues) -> bool {
    style.is_vertical()
        && !style.writing_mode.is_sideways()
        && style.text_orientation == crate::values::TextOrientation::Upright
}

/// `text-orientation`, which lightningcss keeps as tokens.
fn text_orientation_id() -> PropertyId<'static> {
    PropertyId::Custom(CustomPropertyName::Unknown(lightningcss::values::ident::Ident(
        "text-orientation".into(),
    )))
}

fn font_size_adjust_id() -> PropertyId<'static> {
    PropertyId::Custom(CustomPropertyName::Unknown(lightningcss::values::ident::Ident(
        "font-size-adjust".into(),
    )))
}

fn writing_mode_id() -> PropertyId<'static> {
    PropertyId::Custom(CustomPropertyName::Unknown(lightningcss::values::ident::Ident(
        "writing-mode".into(),
    )))
}

/// The display of a box whose writing mode differs from its parent's: an inline box becomes an
/// inline block, which establishes a formatting context in its own writing mode (CSS Writing
/// Modes 4, §3.1). The parts of a table stay what they are, as in Chrome.
fn flow_root_display(display: Display) -> Display {
    use crate::values::{DisplayInside, DisplayOutside};
    match display {
        Display::Normal {
            outside: DisplayOutside::Inline,
            inside: DisplayInside::Flow,
            list_item,
        } => Display::Normal {
            outside: DisplayOutside::Inline,
            inside: DisplayInside::FlowRoot,
            list_item,
        },
        _ => display,
    }
}

/// The decorations drawn over a box's text: its ancestors' (unless it is an atomic inline, a
/// float or absolutely positioned, which they do not reach) and its own.
fn decorations(style: &ComputedValues, parent: &ComputedValues) -> Arc<[crate::values::Decoration]> {
    let atomic = matches!(
        style.display,
        Display::Normal {
            outside: crate::values::DisplayOutside::Inline,
            inside,
            ..
        } if inside != crate::values::DisplayInside::Flow
    );
    let propagates = !(atomic || style.float != Float::None || style.is_out_of_flow());
    let own = (!style.text_decoration_line.is_none()).then(|| crate::values::Decoration {
        line: style.text_decoration_line,
        style: style.text_decoration_style,
        // `currentcolor` is the color the text is filled with, as in Chrome.
        color: style
            .text_decoration_color
            .unwrap_or(style.text_fill_color.unwrap_or(style.color)),
        thickness: style.text_decoration_thickness,
        underline_offset: style.text_underline_offset,
        under: style.text_underline_position == crate::values::UnderlinePosition::Under,
        font_size: style.font_size,
    });
    // Decorations that reach a box which is not inline-level (a block, a table cell, a flex or
    // grid item) are drawn with its font from there on, as in Chrome: an underline of a `div`
    // is as thick and as far down in a heading inside it as the heading's font makes it.
    let resets = propagates
        && parent
            .decorations
            .iter()
            .any(|decoration| (decoration.font_size - style.font_size).abs() > f32::EPSILON)
        && starts_own_font(style, parent);
    let inherited = || -> Arc<[crate::values::Decoration]> {
        if resets {
            parent
                .decorations
                .iter()
                .map(|decoration| crate::values::Decoration {
                    font_size: style.font_size,
                    ..*decoration
                })
                .collect()
        } else {
            Arc::clone(&parent.decorations)
        }
    };
    match (propagates, own) {
        (true, None) => inherited(),
        (false, None) => Arc::from([]),
        (true, Some(own)) => inherited().iter().copied().chain([own]).collect(),
        (false, Some(own)) => Arc::from([own]),
    }
}

/// Whether the decorations a box takes from its ancestors follow its own font: those of a box
/// that is not inline-level (and not `display: contents`), or of a flex or grid item.
fn starts_own_font(style: &ComputedValues, parent: &ComputedValues) -> bool {
    let item = matches!(
        parent.display,
        Display::Normal {
            inside: crate::values::DisplayInside::Flex | crate::values::DisplayInside::Grid,
            ..
        }
    );
    item || match style.display {
        Display::Normal { outside, .. } => outside != crate::values::DisplayOutside::Inline,
        Display::Internal(_) => true,
        Display::None | Display::Contents => false,
    }
}

/// Fixes up values that depend on other properties.
fn finish(style: &mut ComputedValues, is_root: bool) {
    // Spacing given as a percentage is that of the element's own font size.
    if let Some((pixels, fraction)) = style.spacing_percentages[0] {
        style.letter_spacing = pixels + fraction * style.font_size;
    }
    if let Some((pixels, fraction)) = style.spacing_percentages[1] {
        style.word_spacing = pixels + fraction * style.font_size;
    }
    // `-webkit-line-clamp` on a vertical `-webkit-box` (which computes as flex) clamps a block
    // container, as in Chrome.
    if let (Some(lines), crate::values::BoxOrient::Vertical) = (style.webkit_line_clamp, style.box_orient)
        && let Display::Normal {
            outside,
            inside: crate::values::DisplayInside::Flex,
            list_item,
        } = style.display
    {
        style.display = Display::Normal {
            outside,
            inside: crate::values::DisplayInside::FlowRoot,
            list_item,
        };
        style.line_clamp = style.line_clamp.or(Some(lines));
    }
    crate::webkit_box::finish_container(style);
    // `font-size-adjust: from-font` takes the first available font of the element that declares
    // it; its children inherit that font's aspect value.
    if let crate::values::FontSizeAdjust::FromFont(metric, None) = style.font_size_adjust {
        style.font_size_adjust = crate::values::FontSizeAdjust::FromFont(
            metric,
            Some(Arc::new(FontRequest {
                families: Arc::clone(&style.font_family),
                weight: style.font_weight,
                style: style.font_style,
                stretch: style.font_stretch,
                optical_size: optical_size(style),
                size: style.font_size,
                lang: style.lang.clone(),
            })),
        );
    }
    style.border_width_declared = style.border_width;
    for (width, border) in [
        (&mut style.border_width.top, style.border_style.top),
        (&mut style.border_width.right, style.border_style.right),
        (&mut style.border_width.bottom, style.border_style.bottom),
        (&mut style.border_width.left, style.border_style.left),
    ] {
        if !border.is_visible() {
            *width = 0.0;
        }
    }
    // `outline-width` keeps its value with `outline-style: none` (which draws nothing), as in
    // Chrome 157: a child that inherits it with a style of its own draws it (WPT
    // `css-ui/outline-009`).
    if style.is_out_of_flow() {
        style.float = Float::None;
    }
    // `content-visibility: hidden` gives a box size, layout, style and paint containment (CSS
    // Contain 2, §4); without a box of its own it does nothing.
    if matches!(style.display, Display::None | Display::Contents) {
        style.content_hidden = false;
    }
    if style.content_hidden {
        style.contain_size = true;
        style.contain_layout = true;
        style.layout_containment = true;
        style.contain_any = true;
        for overflow in [&mut style.overflow_x, &mut style.overflow_y] {
            if *overflow == crate::values::Overflow::Visible {
                *overflow = crate::values::Overflow::Clip;
            }
        }
    }
    // A footnote is an element in the flow, not the root (CSS GCPM 3, §2.1); it is laid out as
    // a block in the footnote area, where `footnote-display` places its lines.
    if style.footnote
        && (is_root
            || matches!(style.position, Position::Absolute | Position::Fixed)
            || style.running.is_some()
            || matches!(style.display, Display::None | Display::Contents))
    {
        style.footnote = false;
    }
    if is_root
        || style.float != Float::None
        || style.footnote
        || matches!(style.position, Position::Absolute | Position::Fixed)
    {
        style.originally_inline = !is_root
            && matches!(
                style.display,
                Display::Normal {
                    outside: crate::values::DisplayOutside::Inline,
                    ..
                }
            );
        style.display = style.display.blockify();
        if is_root && style.display == Display::Contents {
            style.display = Display::BLOCK;
        }
    }
}

/// Applies `inherit`, `initial` or `unset` to one property.
fn apply_keyword(
    id: &PropertyId<'static>,
    keyword: Keyword,
    style: &mut ComputedValues,
    parent: &ComputedValues,
    initial: &ComputedValues,
) {
    // A logical property takes the keyword to the physical property it stands for in the
    // element's writing mode and direction, as in Chrome.
    if let Some(physical) = compute::physical_id(id, style.writing_mode, style.is_rtl()) {
        apply_keyword(&physical, keyword, style, parent, initial);
        return;
    }
    let inherit = match keyword {
        Keyword::Inherit => true,
        Keyword::Initial => false,
        Keyword::Unset => is_inherited(id),
    };
    // Lengths come scaled to the element's zoom: inherited ones from the parent's, the initial
    // `medium` widths of borders, outlines and column rules from none.
    if inherit {
        copy_property(id, parent, style);
        // A border width comes as the parent's cascade gives it, even where the parent's border
        // style makes its own 0, as in Chrome.
        match id {
            PropertyId::BorderTopWidth => style.border_width.top = parent.border_width_declared.top,
            PropertyId::BorderRightWidth => style.border_width.right = parent.border_width_declared.right,
            PropertyId::BorderBottomWidth => style.border_width.bottom = parent.border_width_declared.bottom,
            PropertyId::BorderLeftWidth => style.border_width.left = parent.border_width_declared.left,
            _ => {}
        }
        let ratio = style.zoom / parent.zoom;
        if (ratio - 1.0).abs() > 1e-6 {
            zoom::rezoom_property(id, style, ratio);
        }
    } else {
        copy_property(id, initial, style);
        zoom::initial_widths(id, style);
    }
}

fn is_inherited(id: &PropertyId<'_>) -> bool {
    matches!(
        id,
        PropertyId::Color
            | PropertyId::FontFamily
            | PropertyId::FontSize
            | PropertyId::FontWeight
            | PropertyId::FontStyle
            | PropertyId::FontStretch
            | PropertyId::FontVariantCaps
            | PropertyId::TextDecorationSkipInk(_)
            | PropertyId::ImageRendering
            | PropertyId::TextRendering
            | PropertyId::LineHeight
            | PropertyId::LetterSpacing
            | PropertyId::WordSpacing
            | PropertyId::TextAlign
            | PropertyId::Direction
            | PropertyId::TextIndent
            | PropertyId::TextTransform
            | PropertyId::WhiteSpace
            | PropertyId::OverflowWrap
            | PropertyId::WordWrap
            | PropertyId::WordBreak
            | PropertyId::Visibility
            | PropertyId::AccentColor
            | PropertyId::ColorScheme
            | PropertyId::ListStyleType
            | PropertyId::ListStylePosition
            | PropertyId::ListStyleImage
            | PropertyId::BorderSpacing
            | PropertyId::Hyphens(_)
            | PropertyId::LineBreak
            | PropertyId::TextShadow
            | PropertyId::TextAlignLast(_)
            | PropertyId::TextJustify
            | PropertyId::TabSize(_)
            | PropertyId::TextEmphasisStyle(_)
            | PropertyId::TextEmphasisColor(_)
            | PropertyId::TextEmphasisPosition(_)
    ) || matches!(id, PropertyId::Custom(CustomPropertyName::Unknown(name)) if matches!(
        name.0.as_ref(),
        "orphans" | "widows" | "border-collapse" | "caption-side" | "quotes" | "hyphenate-character" | "hyphenate-limit-chars" | "font-variant-numeric" | "font-feature-settings"
            | "text-underline-offset" | "text-underline-position" | "empty-cells" | "font-variation-settings"
            | "font-optical-sizing" | "font-variant" | "font-variant-ligatures" | "font-variant-east-asian"
            | "font-variant-position" | "font-variant-alternates" | "font-kerning" | "font-synthesis"
            | "font-synthesis-weight" | "font-synthesis-style" | "font-synthesis-small-caps" | "font-size-adjust"
            | "white-space-collapse" | "text-wrap-mode" | "text-wrap-style" | "text-wrap" | "image-orientation"
            | "ruby-position" | "ruby-align" | "text-box-edge" | "writing-mode" | "-webkit-writing-mode" | "text-orientation"
            | "text-spacing-trim" | "text-autospace" | "text-fit" | "hanging-punctuation" | "scrollbar-color"
    ))
}

/// Copies the value of one property from `source` to `style`.
/// Copies a longhand of `border-image` from `source`, whose border image may not be made yet.
fn copy_border_image(
    style: &mut ComputedValues,
    source: &ComputedValues,
    copy: impl FnOnce(&mut crate::values::BorderImage, &crate::values::BorderImage),
) {
    let initial = crate::values::BorderImage::default();
    let from = source.border_image.as_deref().unwrap_or(&initial);
    copy(
        Arc::make_mut(style.border_image.get_or_insert_with(Default::default)),
        from,
    );
}

/// Copies a longhand of `mask` from `source`, whose mask layers may not be made yet.
fn copy_mask(
    style: &mut ComputedValues,
    source: &ComputedValues,
    copy: impl FnOnce(&mut crate::values::MaskLayers, &crate::values::MaskLayers),
) {
    let initial = crate::values::MaskLayers::default();
    let from = source.mask.as_deref().unwrap_or(&initial);
    copy(
        Arc::make_mut(style.mask.get_or_insert_with(Default::default)),
        from,
    );
}

fn copy_property(id: &PropertyId<'static>, source: &ComputedValues, style: &mut ComputedValues) {
    macro_rules! copy {
        ($($field:ident).+) => { style.$($field).+ = source.$($field).+.clone() };
    }
    match id {
        PropertyId::Display => {
            copy!(display);
            copy!(ruby_display);
        }
        PropertyId::Position => {
            copy!(position);
            copy!(running);
        }
        PropertyId::Top => copy!(inset.top),
        PropertyId::Right => copy!(inset.right),
        PropertyId::Bottom => copy!(inset.bottom),
        PropertyId::Left => copy!(inset.left),
        PropertyId::ZIndex => copy!(z_index),
        PropertyId::BoxSizing(_) => copy!(box_sizing),
        PropertyId::Resize => copy!(resize),
        PropertyId::Width => copy!(width),
        PropertyId::Height => copy!(height),
        PropertyId::MinWidth => copy!(min_width),
        PropertyId::MinHeight => copy!(min_height),
        PropertyId::MaxWidth => copy!(max_width),
        PropertyId::MaxHeight => copy!(max_height),
        PropertyId::MarginTop => copy!(margin.top),
        PropertyId::MarginRight => copy!(margin.right),
        PropertyId::MarginBottom => copy!(margin.bottom),
        PropertyId::MarginLeft => copy!(margin.left),
        PropertyId::PaddingTop => copy!(padding.top),
        PropertyId::PaddingRight => copy!(padding.right),
        PropertyId::PaddingBottom => copy!(padding.bottom),
        PropertyId::PaddingLeft => copy!(padding.left),
        PropertyId::BorderTopWidth => copy!(border_width.top),
        PropertyId::BorderRightWidth => copy!(border_width.right),
        PropertyId::BorderBottomWidth => copy!(border_width.bottom),
        PropertyId::BorderLeftWidth => copy!(border_width.left),
        PropertyId::BorderTopStyle => copy!(border_style.top),
        PropertyId::BorderRightStyle => copy!(border_style.right),
        PropertyId::BorderBottomStyle => copy!(border_style.bottom),
        PropertyId::BorderLeftStyle => copy!(border_style.left),
        PropertyId::BorderTopColor => copy!(border_color.top),
        PropertyId::BorderRightColor => copy!(border_color.right),
        PropertyId::BorderBottomColor => copy!(border_color.bottom),
        PropertyId::BorderLeftColor => copy!(border_color.left),
        PropertyId::BorderTopLeftRadius(_) => copy!(border_radius.top_left),
        PropertyId::BorderTopRightRadius(_) => copy!(border_radius.top_right),
        PropertyId::BorderBottomRightRadius(_) => copy!(border_radius.bottom_right),
        PropertyId::BorderBottomLeftRadius(_) => copy!(border_radius.bottom_left),
        PropertyId::AlignContent(_) => {
            copy!(align_content);
            copy!(align_content_unsafe);
            copy!(align_content_safe);
        }
        PropertyId::AlignItems(_) => {
            copy!(align_items);
            copy!(align_items_flags);
        }
        PropertyId::AlignSelf(_) => {
            copy!(align_self);
            copy!(align_self_flags);
        }
        PropertyId::JustifyContent(_) => {
            copy!(justify_content);
            copy!(justify_content_safe);
        }
        PropertyId::FlexDirection(_) => copy!(flex_direction),
        PropertyId::FlexWrap(_) => {
            copy!(flex_wrap);
            copy!(flex_balance);
        }
        PropertyId::AspectRatio => {
            copy!(aspect_ratio);
            copy!(aspect_ratio_auto);
        }
        PropertyId::OverflowWrap | PropertyId::WordWrap => copy!(overflow_wrap),
        PropertyId::WordBreak => copy!(word_break),
        PropertyId::TextDecorationLine(_) => copy!(text_decoration_line),
        PropertyId::TextDecorationStyle(_) => copy!(text_decoration_style),
        PropertyId::TextDecorationColor(_) => copy!(text_decoration_color),
        PropertyId::Color => copy!(color),
        PropertyId::BackgroundColor => {
            copy!(background_color);
            copy!(background_color_current);
            // An inherited `currentcolor` is the element's own color.
            if let Some(tokens) = &style.background_color_current {
                style.background_color =
                    crate::current_color::resolve(tokens).unwrap_or(crate::values::Rgba::TRANSPARENT);
            }
        }
        PropertyId::BoxShadow(_) => copy!(box_shadow),
        PropertyId::Transform(_) => copy!(transform),
        PropertyId::Translate => copy!(translate),
        PropertyId::Rotate => copy!(rotate),
        PropertyId::Scale => copy!(scale),
        PropertyId::TransformOrigin(_) => copy!(transform_origin),
        PropertyId::TransformBox => copy!(transform_box),
        PropertyId::TransformStyle(_) => copy!(preserve_3d),
        PropertyId::BackdropFilter(_) => copy!(backdrop_filter),
        PropertyId::Perspective(_) => copy!(perspective),
        PropertyId::PerspectiveOrigin(_) => copy!(perspective_origin),
        PropertyId::BackgroundImage => copy!(background_image),
        PropertyId::BackgroundPositionX => copy!(background_position_x),
        PropertyId::BackgroundPositionY => copy!(background_position_y),
        PropertyId::BackgroundSize => copy!(background_size),
        PropertyId::BackgroundRepeat => copy!(background_repeat),
        PropertyId::BackgroundOrigin => copy!(background_origin),
        PropertyId::BackgroundAttachment => {
            copy!(background_fixed);
            copy!(background_local);
        }
        PropertyId::BackgroundClip(_) => {
            copy!(background_clip);
            copy!(background_clip_text);
            copy!(background_clip_border_area);
        }
        PropertyId::FontFamily => copy!(font_family),
        PropertyId::FontSize => copy!(font_size),
        PropertyId::FontWeight => copy!(font_weight),
        PropertyId::FontStyle => copy!(font_style),
        PropertyId::FontStretch => copy!(font_stretch),
        PropertyId::FontVariantCaps => copy!(font_variant.caps),
        PropertyId::TextDecorationSkipInk(_) => copy!(text_decoration_skip_ink),
        PropertyId::BoxOrient(_) => copy!(box_orient),
        PropertyId::ImageRendering => copy!(image_rendering),
        PropertyId::TextRendering => copy!(text_rendering_speed),
        PropertyId::LineHeight => copy!(line_height),
        PropertyId::LetterSpacing => copy!(letter_spacing),
        PropertyId::WordSpacing => copy!(word_spacing),
        PropertyId::TextAlign => {
            copy!(text_align);
            copy!(align_blocks);
        }
        PropertyId::Direction => copy!(direction),
        PropertyId::UnicodeBidi => copy!(unicode_bidi),
        PropertyId::TextIndent => {
            copy!(text_indent);
            copy!(text_indent_each_line);
            copy!(text_indent_hanging);
        }
        PropertyId::TextTransform => copy!(text_transform),
        PropertyId::TextDecorationThickness => copy!(text_decoration_thickness),
        PropertyId::TextOverflow(_) => copy!(text_overflow),
        PropertyId::OverflowX => {
            copy!(overflow_x);
            copy!(scrollbar_x);
        }
        PropertyId::OverflowY => {
            copy!(overflow_y);
            copy!(scrollbar_y);
        }
        PropertyId::BoxDecorationBreak(_) => copy!(box_decoration_break),
        PropertyId::ContainerType => copy!(container_type),
        PropertyId::ContainerName => copy!(container_names),
        PropertyId::TextShadow => copy!(text_shadow),
        PropertyId::TextAlignLast(_) => copy!(text_align_last),
        PropertyId::TextJustify => copy!(text_justify),
        PropertyId::TabSize(_) => copy!(tab_size),
        PropertyId::TextEmphasisStyle(_) => copy!(text_emphasis_style),
        PropertyId::TextEmphasisColor(_) => copy!(text_emphasis_color),
        PropertyId::TextEmphasisPosition(_) => copy!(text_emphasis_position),
        PropertyId::OutlineWidth => copy!(outline_width),
        PropertyId::OutlineStyle => copy!(outline_style),
        PropertyId::OutlineColor => copy!(outline_color),
        PropertyId::WhiteSpace => copy!(white_space),
        PropertyId::VerticalAlign => copy!(vertical_align),
        PropertyId::Visibility => copy!(visibility),
        PropertyId::Appearance(_) => copy!(appearance),
        PropertyId::AccentColor => copy!(accent_color),
        PropertyId::ColorScheme => copy!(dark),
        PropertyId::Opacity => copy!(opacity),
        PropertyId::MixBlendMode => copy!(mix_blend_mode),
        PropertyId::ClipPath(_) => copy!(clip_path),
        PropertyId::Filter(_) => copy!(filter),
        PropertyId::BorderImageSource => {
            copy_border_image(style, source, |to, from| to.source = from.source.clone());
        }
        PropertyId::BorderImageSlice => copy_border_image(style, source, |to, from| {
            to.slice = from.slice;
            to.fill = from.fill;
        }),
        PropertyId::BorderImageWidth => {
            copy_border_image(style, source, |to, from| to.width.clone_from(&from.width));
        }
        PropertyId::BorderImageOutset => copy_border_image(style, source, |to, from| to.outset = from.outset),
        PropertyId::BorderImageRepeat => copy_border_image(style, source, |to, from| to.repeat = from.repeat),
        PropertyId::MaskImage(_) => copy_mask(style, source, |to, from| to.images = from.images.clone()),
        PropertyId::MaskMode => copy_mask(style, source, |to, from| to.modes = from.modes.clone()),
        PropertyId::MaskRepeat(_) => copy_mask(style, source, |to, from| to.repeats = from.repeats.clone()),
        PropertyId::MaskPositionX => {
            copy_mask(style, source, |to, from| to.position_x = from.position_x.clone());
        }
        PropertyId::MaskPositionY => {
            copy_mask(style, source, |to, from| to.position_y = from.position_y.clone());
        }
        PropertyId::MaskPosition(_) => copy_mask(style, source, |to, from| {
            to.position_x = from.position_x.clone();
            to.position_y = from.position_y.clone();
        }),
        PropertyId::MaskClip(_) => copy_mask(style, source, |to, from| {
            to.clips = from.clips.clone();
            to.svg_clips = from.svg_clips.clone();
        }),
        PropertyId::MaskOrigin(_) => copy_mask(style, source, |to, from| {
            to.origins = from.origins.clone();
            to.svg_origins = from.svg_origins.clone();
        }),
        PropertyId::MaskSize(_) => copy_mask(style, source, |to, from| to.sizes = from.sizes.clone()),
        PropertyId::MaskComposite | PropertyId::WebKitMaskComposite => {
            copy_mask(style, source, |to, from| to.composites = from.composites.clone());
        }
        PropertyId::ListStyleType => copy!(list_style_type),
        PropertyId::ListStylePosition => copy!(list_style_position),
        PropertyId::ListStyleImage => copy!(list_style_image),
        PropertyId::BorderSpacing => copy!(border_spacing),
        PropertyId::FlexGrow(_) => copy!(flex_grow),
        PropertyId::FlexShrink(_) => copy!(flex_shrink),
        PropertyId::FlexBasis(_) => copy!(flex_basis),
        PropertyId::Order(_) => copy!(order),
        PropertyId::RowGap => copy!(row_gap),
        PropertyId::ColumnGap => copy!(column_gap),
        PropertyId::GridTemplateColumns => copy!(grid_template_columns),
        PropertyId::GridTemplateRows => copy!(grid_template_rows),
        PropertyId::GridTemplateAreas => copy!(grid_template_areas),
        PropertyId::GridAutoRows => copy!(grid_auto_rows),
        PropertyId::GridAutoColumns => copy!(grid_auto_columns),
        PropertyId::GridAutoFlow => {
            copy!(grid_auto_flow_column);
            copy!(grid_auto_flow_dense);
        }
        PropertyId::GridRowStart => copy!(grid_row_start),
        PropertyId::GridRowEnd => copy!(grid_row_end),
        PropertyId::GridColumnStart => copy!(grid_column_start),
        PropertyId::GridColumnEnd => copy!(grid_column_end),
        PropertyId::JustifyItems => {
            copy!(justify_items);
            copy!(justify_items_block);
        }
        PropertyId::Hyphens(_) => copy!(hyphens),
        PropertyId::LineBreak => copy!(line_break),
        PropertyId::FontPalette => copy!(font_palette),
        PropertyId::JustifySelf => {
            copy!(justify_self);
            copy!(justify_self_block);
        }
        PropertyId::Custom(CustomPropertyName::Unknown(name)) => match name.0.as_ref() {
            "orphans" => copy!(orphans),
            "widows" => copy!(widows),
            "border-collapse" => copy!(border_collapse),
            "quotes" => copy!(quotes),
            "hyphenate-character" => copy!(hyphenate_character),
            "hyphenate-limit-chars" => copy!(hyphenate_limit_chars),
            "break-before" | "page-break-before" => copy!(break_before),
            "break-after" | "page-break-after" => copy!(break_after),
            "break-inside" | "page-break-inside" => copy!(break_inside),
            "content" => copy!(content),
            "float" => {
                copy!(float);
                copy!(footnote);
            }
            "footnote-display" => copy!(footnote_display),
            "footnote-policy" => copy!(footnote_policy),
            "float-reference" => copy!(float_reference),
            "shape-outside" => copy!(shape_outside),
            "shape-margin" => copy!(shape_margin),
            "shape-image-threshold" => copy!(shape_image_threshold),
            "clear" => copy!(clear),
            "initial-letter" => copy!(initial_letter),
            "table-layout" => copy!(table_layout),
            "page" => copy!(page),
            "string-set" => copy!(string_set),
            "bookmark-level" => copy!(bookmark_level),
            "bookmark-label" => copy!(bookmark_label),
            "bookmark-state" => copy!(bookmark_closed),
            "column-count" => copy!(column_count),
            "column-width" => copy!(column_width),
            "columns" => {
                copy!(column_count);
                copy!(column_width);
                copy!(column_height);
                copy!(column_wrap);
            }
            "column-rule-width" => copy!(column_rule_width),
            "column-rule-style" => copy!(column_rule_style),
            "column-rule-color" => copy!(column_rule_color),
            "column-rule" => {
                copy!(column_rule_width);
                copy!(column_rule_style);
                copy!(column_rule_color);
            }
            "column-fill" => copy!(column_fill),
            "-webkit-text-fill-color" => copy!(text_fill_color),
            "column-span" => copy!(column_span),
            "column-height" => copy!(column_height),
            "column-wrap" => copy!(column_wrap),
            "flex-line-count" => copy!(flex_line_count),
            "-fagus-repeat" => copy!(repeat),
            "-fagus-layer" => copy!(layer),
            "text-underline-offset" => copy!(text_underline_offset),
            "outline-offset" => copy!(outline_offset),
            "empty-cells" => copy!(empty_cells),
            "field-sizing" => copy!(field_sizing_content),
            "scrollbar-width" => copy!(scrollbar_width),
            "scrollbar-gutter" => copy!(scrollbar_gutter),
            "scrollbar-color" => copy!(scrollbar_color),
            "frame-sizing" => copy!(frame_sizing_content_height),
            "text-underline-position" => copy!(text_underline_position),
            "font-variation-settings" => copy!(font_variation_settings),
            "font-optical-sizing" => copy!(font_optical_sizing),
            "font-variant" => copy!(font_variant),
            "font-variant-ligatures" => copy!(font_variant.ligatures),
            "font-variant-numeric" => copy!(font_variant.numeric),
            "font-variant-east-asian" => copy!(font_variant.east_asian),
            "font-variant-position" => copy!(font_variant.position),
            "font-variant-alternates" => copy!(font_variant.historical_forms),
            "font-kerning" => copy!(font_kerning),
            "font-feature-settings" => copy!(font_feature_settings),
            "font-synthesis" => copy!(font_synthesis),
            "font-synthesis-weight" => copy!(font_synthesis.weight),
            "font-synthesis-style" => copy!(font_synthesis.style),
            "font-synthesis-small-caps" => copy!(font_synthesis.small_caps),
            "font-size-adjust" => copy!(font_size_adjust),
            "image-orientation" => copy!(image_orientation),
            "isolation" => copy!(isolation),
            "background-blend-mode" => copy!(background_blend_mode),
            "-webkit-line-clamp" => copy!(webkit_line_clamp),
            "white-space-collapse" => copy!(white_space.collapse),
            "text-wrap-mode" => copy!(white_space.nowrap),
            "text-wrap-style" => copy!(text_wrap_style),
            "text-wrap" => {
                copy!(white_space.nowrap);
                copy!(text_wrap_style);
            }
            "clip" => copy!(clip),
            "object-fit" => copy!(object_fit),
            "object-position" => copy!(object_position),
            "ruby-position" => copy!(ruby_position),
            "ruby-align" => copy!(ruby_align),
            "text-box-trim" => copy!(text_box_trim),
            "text-box-edge" => copy!(text_box_edge),
            "text-box" => {
                copy!(text_box_trim);
                copy!(text_box_edge);
            }
            "writing-mode" | "-webkit-writing-mode" => copy!(writing_mode),
            "caption-side" => copy!(caption_side),
            "counter-reset" => copy!(counter_reset),
            "counter-increment" => copy!(counter_increment),
            "counter-set" => copy!(counter_set),
            "contain" => {
                copy!(contain_size);
                copy!(contain_inline_size);
                copy!(contain_any);
                copy!(contain_layout);
                copy!(layout_containment);
            }
            "will-change" => copy!(will_change_contains),
            "scroll-marker-group" => copy!(scroll_marker_group),
            "contain-intrinsic-size" => copy!(contain_intrinsic_size),
            "overflow-clip-margin" => copy!(overflow_clip_margin),
            "content-visibility" => copy!(content_hidden),
            "margin-trim" => copy!(margin_trim_block),
            "contain-intrinsic-inline-size" if style.is_vertical() => {
                style.contain_intrinsic_size.1 = source.contain_intrinsic_size.1;
            }
            "contain-intrinsic-block-size" if style.is_vertical() => {
                style.contain_intrinsic_size.0 = source.contain_intrinsic_size.0;
            }
            "contain-intrinsic-width" | "contain-intrinsic-inline-size" => {
                style.contain_intrinsic_size.0 = source.contain_intrinsic_size.0;
            }
            "contain-intrinsic-height" | "contain-intrinsic-block-size" => {
                style.contain_intrinsic_size.1 = source.contain_intrinsic_size.1;
            }
            "text-orientation" => copy!(text_orientation),
            "text-combine-upright" => copy!(text_combine_upright),
            "text-spacing-trim" => copy!(text_spacing_trim),
            "text-autospace" => copy!(text_autospace),
            "hanging-punctuation" => copy!(hanging_punctuation),
            "text-fit" => copy!(text_fit),
            _ => {}
        },
        _ => {}
    }
}

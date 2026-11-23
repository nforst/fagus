//! Flattening and indexing of the rules of several stylesheets.

use std::sync::Arc;

use fagus_dom::{Document, NodeId};
use lightningcss::properties::Property as LightningProperty;
use lightningcss::rules::counter_style::CounterStyleRule;
use lightningcss::rules::font_face::FontFaceRule;
use lightningcss::rules::page::PageMarginRule;
use lightningcss::rules::property::PropertyRule;
use lightningcss::rules::style::StyleRule;
use lightningcss::rules::supports::SupportsCondition;
use lightningcss::rules::{CssRule, CssRuleList};
use lightningcss::stylesheet::ParserOptions;
use rustc_hash::FxHashMap;

use crate::matching::{Ancestors, Matcher, Nesting, PseudoElementKind};
use crate::{Component, DeclarationBlock, MediaEnvironment, Origin, Selector, Stylesheet};

/// The bands of the cascade, from weakest to strongest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CascadeBand {
    UserAgentNormal,
    AuthorNormal,
    /// Declarations in a `style` attribute.
    InlineNormal,
    AuthorImportant,
    InlineImportant,
    UserAgentImportant,
}

impl CascadeBand {
    pub fn of(origin: Origin, important: bool) -> Self {
        match (origin, important) {
            (Origin::UserAgent, false) => Self::UserAgentNormal,
            (Origin::Author, false) => Self::AuthorNormal,
            (Origin::Author, true) => Self::AuthorImportant,
            (Origin::UserAgent, true) => Self::UserAgentImportant,
        }
    }
}

/// The rank of a declaration in the cascade. Compared lexicographically: the greater wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Precedence {
    pub band: CascadeBand,
    /// Rank of the cascade layer, already reversed for important declarations.
    pub layer: u32,
    pub specificity: u32,
    /// How near the root of its scope a scoped rule's element lies: `u32::MAX` less the
    /// generations between them, 0 for rules without a scope (CSS Cascade 6, §6.4).
    pub proximity: u32,
    pub order: u32,
}

/// An `@scope` rule around rules (CSS Cascade 6, §2.5): they apply to the elements in a scope,
/// below a root and above its limits.
#[derive(Debug)]
pub struct Scope {
    /// What the roots are (`<scope-start>`); without it, the parent of the `<style>` element.
    pub start: Option<crate::SelectorList>,
    /// What the limits are (`<scope-end>`).
    pub end: Option<crate::SelectorList>,
    /// The `<style>` element the rule is in.
    pub owner: Option<NodeId>,
    /// The `@scope` rule this one is nested in: its roots lie in that one's scope, and
    /// `:scope` in its `<scope-start>` is that one's root.
    pub parent: Option<Arc<Scope>>,
}

/// A style rule with a single complex selector.
#[derive(Debug)]
pub struct Rule {
    pub selector: Selector,
    pub specificity: u32,
    pub declarations: Arc<DeclarationBlock>,
    pub origin: Origin,
    /// Rank of the cascade layer for normal declarations (higher wins). Unlayered rules have the
    /// highest rank.
    pub layer: u32,
    /// Position of the rule in the cascade order of all stylesheets.
    pub order: u32,
    /// What `&` refers to, for nested rules.
    pub nesting: Option<Arc<Nesting>>,
    /// The pseudo-element the rule styles, if any.
    pub pseudo: Option<PseudoElementKind>,
    /// The `@container` queries around the rule, which all must hold for it to apply.
    pub container: Option<Arc<[ContainerQuery]>>,
    /// The `@scope` rule around the rule.
    pub scope: Option<Arc<Scope>>,
    /// Source line (1-based).
    pub line: u32,
}

/// The query of an `@container` rule: the containers it asks (by name, or the nearest) and
/// what it asks of their size.
#[derive(Clone, Debug)]
pub struct ContainerQuery {
    pub name: Option<String>,
    pub condition: Option<lightningcss::rules::container::ContainerCondition<'static>>,
}

impl Rule {
    /// The rank of this rule's normal or important declarations.
    pub fn precedence(&self, important: bool) -> Precedence {
        Precedence {
            band: CascadeBand::of(self.origin, important),
            layer: if important {
                u32::MAX - self.layer
            } else {
                self.layer
            },
            specificity: self.specificity,
            proximity: 0,
            order: self.order,
        }
    }
}

/// A rule that matched an element, with how near its element lies to the root of its scope.
#[derive(Clone, Copy, Debug)]
pub struct MatchedRule<'a> {
    pub rule: &'a Rule,
    /// As in [`Precedence::proximity`].
    pub proximity: u32,
}

impl MatchedRule<'_> {
    /// The rank of the rule's normal or important declarations for this element.
    pub fn precedence(&self, important: bool) -> Precedence {
        Precedence {
            proximity: self.proximity,
            ..self.rule.precedence(important)
        }
    }
}

impl std::ops::Deref for MatchedRule<'_> {
    type Target = Rule;

    fn deref(&self) -> &Rule {
        self.rule
    }
}

/// An `@page` rule.
#[derive(Debug, Clone)]
pub struct PageRuleData {
    pub selectors: Vec<crate::PageSelector>,
    pub declarations: DeclarationBlock,
    pub margin_rules: Vec<PageMarginRule<'static>>,
    /// The declarations of its `@footnote` rules (CSS Generated Content for Paged Media 3,
    /// §2.2), in order.
    pub footnote_rules: Vec<DeclarationBlock>,
    pub origin: Origin,
    pub layer: u32,
    pub order: u32,
}

/// The rules of all stylesheets of a document, flattened and indexed.
#[derive(Debug, Default)]
pub struct RuleSet {
    rules: Vec<Rule>,
    by_id: FxHashMap<String, Vec<u32>>,
    by_class: FxHashMap<String, Vec<u32>>,
    by_tag: FxHashMap<String, Vec<u32>>,
    universal: Vec<u32>,
    pseudo: Vec<u32>,
    page_rules: Vec<PageRuleData>,
    font_faces: Vec<(FontFaceRule<'static>, Origin)>,
    property_rules: Vec<PropertyRule<'static>>,
    counter_styles: Vec<CounterStyleRule<'static>>,
    font_palette_values: Vec<lightningcss::rules::font_palette_values::FontPaletteValuesRule<'static>>,
    position_tries: Vec<lightningcss::rules::position_try::PositionTryRule<'static>>,
    keyframes: Vec<lightningcss::rules::keyframes::KeyframesRule<'static>>,
    /// Whether rules depend on the sizes of containers (`@container`).
    container_queries: bool,
    /// Whether there are `@media` rules for the screen only (see [`RuleSet::has_screen_rules`]).
    screen_rules: bool,
}

impl RuleSet {
    /// Whether rules other than the user agent's style pseudo-elements of kind `kind`
    /// (`::scroll-button(*)` those of every scroll button): only then may such pseudo-elements
    /// set the `content` that makes them generate a box.
    pub fn authors_pseudo(&self, kind: PseudoElementKind) -> bool {
        self.pseudo.iter().any(|&index| {
            let rule = &self.rules[index as usize];
            rule.origin != Origin::UserAgent && rule.pseudo.is_some_and(|own| own.styles(kind))
        })
    }

    /// Whether rules depend on the sizes of containers, which layout tells (`@container`).
    pub fn has_container_queries(&self) -> bool {
        self.container_queries
    }

    /// Whether `@media` rules apply on the screen Chrome loads the document on and not in print,
    /// so that the document is laid out otherwise there: Chrome keeps what it computed in that
    /// layout for some things it prints (the offsets of sticky boxes).
    pub fn has_screen_rules(&self) -> bool {
        self.screen_rules
    }

    /// Flattens the rules of `sheets` (in cascade order) for a document printed in `environment`.
    pub fn new(sheets: &[&Stylesheet], environment: &MediaEnvironment) -> Self {
        let mut builder = Builder {
            environment,
            set: Self::default(),
            layers: LayerTree::default(),
            layer_nodes: Vec::new(),
        };
        for sheet in sheets {
            let context = WalkContext {
                origin: sheet.origin,
                layer: 0,
                nesting: None,
                container: None,
                scope: None,
                owner: sheet.owner,
            };
            builder.walk(&sheet.sheet.rules, &context);
        }
        builder.finish()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn page_rules(&self) -> &[PageRuleData] {
        &self.page_rules
    }

    /// The declarations of the `@position-try` rule named `name` (a dashed identifier), the
    /// last of that name in cascade order.
    pub fn position_try(&self, name: &str) -> Option<&lightningcss::declaration::DeclarationBlock<'static>> {
        self.position_tries
            .iter()
            .rev()
            .find(|rule| rule.name.0.as_ref() == name)
            .map(|rule| &rule.declarations)
    }

    /// The `@keyframes` rule named `name`, the last of that name in cascade order (CSS
    /// Animations 1, §3), with or without a vendor prefix.
    pub fn keyframes(&self, name: &str) -> Option<&lightningcss::rules::keyframes::KeyframesRule<'static>> {
        use lightningcss::rules::keyframes::KeyframesName;
        self.keyframes.iter().rev().find(|rule| match &rule.name {
            KeyframesName::Ident(ident) => ident.0.as_ref() == name,
            KeyframesName::Custom(string) => string.as_ref() == name,
        })
    }

    /// Whether any `@keyframes` rule applies.
    pub fn has_keyframes(&self) -> bool {
        !self.keyframes.is_empty()
    }

    /// The `@counter-style` rules, in cascade order: of two with one name, the later counts.
    pub fn counter_styles(&self) -> &[CounterStyleRule<'static>] {
        &self.counter_styles
    }

    /// The `@font-palette-values` rules, in cascade order.
    pub fn font_palette_values(
        &self,
    ) -> &[lightningcss::rules::font_palette_values::FontPaletteValuesRule<'static>] {
        &self.font_palette_values
    }

    pub fn font_faces(&self) -> &[(FontFaceRule<'static>, Origin)] {
        &self.font_faces
    }

    pub fn property_rules(&self) -> &[PropertyRule<'static>] {
        &self.property_rules
    }

    /// The rules that match `element` (or its pseudo-element), in no particular order.
    /// All style rules, in cascade order.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn matching(
        &self,
        document: &Document,
        element: NodeId,
        pseudo: Option<PseudoElementKind>,
    ) -> Vec<MatchedRule<'_>> {
        self.matching_in(document, None, element, pseudo)
    }

    /// The rules that match `element` (or its pseudo-element), as [`RuleSet::matching`] finds
    /// them, with the ancestors of the document's elements known.
    pub fn matching_in(
        &self,
        document: &Document,
        ancestors: Option<&Ancestors>,
        element: NodeId,
        pseudo: Option<PseudoElementKind>,
    ) -> Vec<MatchedRule<'_>> {
        let matcher = match ancestors {
            Some(ancestors) => Matcher::with_ancestors(document, ancestors),
            None => Matcher::new(document),
        };
        let mut matched = Vec::new();
        let mut test = |index: u32| {
            let rule = &self.rules[index as usize];
            let admitted = match (rule.pseudo, pseudo) {
                (Some(own), Some(requested)) => own.styles(requested),
                (own, requested) => own == requested,
            };
            if !admitted {
                return;
            }
            let nesting = rule.nesting.as_deref();
            let proximity = match &rule.scope {
                None => matcher
                    .matches(&rule.selector, element, pseudo, nesting)
                    .then_some(0),
                Some(scope) => matcher
                    .scoped(&rule.selector, element, pseudo, nesting, scope)
                    .map(|generations| u32::MAX - generations),
            };
            if let Some(proximity) = proximity {
                matched.push(MatchedRule { rule, proximity });
            }
        };
        if pseudo.is_some() {
            self.pseudo.iter().copied().for_each(&mut test);
            return matched;
        }
        let Some(data) = document.element(element) else {
            return matched;
        };
        self.universal.iter().copied().for_each(&mut test);
        // HTML names are lowercase already; others keep their case.
        let name: &str = data.local_name();
        let lowered;
        let tag = if data.is_html() && name.bytes().any(|byte| byte.is_ascii_uppercase()) {
            lowered = name.to_ascii_lowercase();
            lowered.as_str()
        } else {
            name
        };
        if let Some(rules) = self.by_tag.get(tag) {
            rules.iter().copied().for_each(&mut test);
        }
        if let Some(id) = &data.id
            && let Some(rules) = self.by_id.get(id)
        {
            rules.iter().copied().for_each(&mut test);
        }
        for class in &data.classes {
            if let Some(rules) = self.by_class.get(class) {
                rules.iter().copied().for_each(&mut test);
            }
        }
        matched
    }
}

struct WalkContext {
    origin: Origin,
    /// Node in the layer tree; 0 is the root (unlayered).
    layer: usize,
    nesting: Option<Arc<Nesting>>,
    /// The `@container` queries the rules are in.
    container: Option<Arc<[ContainerQuery]>>,
    /// The `@scope` rule the rules are in.
    scope: Option<Arc<Scope>>,
    /// The `<style>` element of the stylesheet.
    owner: Option<NodeId>,
}

struct Builder<'e> {
    environment: &'e MediaEnvironment,
    set: RuleSet,
    layers: LayerTree,
    /// Layer tree node of every rule, to assign layer ranks at the end.
    layer_nodes: Vec<usize>,
}

impl Builder<'_> {
    fn walk(&mut self, rules: &CssRuleList<'static>, context: &WalkContext) {
        for rule in &rules.0 {
            match rule {
                CssRule::Style(style) => self.style_rule(style, context),
                CssRule::Nesting(nesting) => self.style_rule(&nesting.style, context),
                CssRule::NestedDeclarations(nested) => {
                    // Declarations after nested rules apply to the parent selector.
                    if let Some(parent) = &context.nesting {
                        let declarations = Arc::new(nested.declarations.clone());
                        for selector in &parent.selectors.0 {
                            self.add(
                                selector.clone(),
                                &declarations,
                                context,
                                parent.parent.clone(),
                                nested.loc.line,
                            );
                        }
                    }
                }
                CssRule::Media(media) => {
                    if self.environment.matches_list(&media.query) {
                        self.walk(&media.rules, context);
                    } else if self.environment.matches_list_on_screen_only(&media.query)
                        && !media.rules.0.is_empty()
                    {
                        self.set.screen_rules = true;
                    }
                }
                CssRule::Supports(supports) => {
                    if supports_condition(&supports.condition) {
                        self.walk(&supports.rules, context);
                    }
                }
                CssRule::Container(container) => {
                    // Style queries are answered by the styles alone, without a layout.
                    if container.condition.as_ref().is_none_or(crate::asks_size) {
                        self.set.container_queries = true;
                    }
                    let query = ContainerQuery {
                        name: container.name.as_ref().map(|name| name.0.0.to_string()),
                        condition: container.condition.clone(),
                    };
                    let queries: Vec<ContainerQuery> = context
                        .container
                        .iter()
                        .flat_map(|queries| queries.iter().cloned())
                        .chain([query])
                        .collect();
                    let inner = WalkContext {
                        origin: context.origin,
                        layer: context.layer,
                        nesting: context.nesting.clone(),
                        container: Some(queries.into()),
                        scope: context.scope.clone(),
                        owner: context.owner,
                    };
                    self.walk(&container.rules, &inner);
                }
                CssRule::LayerStatement(statement) => {
                    for name in &statement.names {
                        self.layers
                            .child_path(context.layer, name.0.iter().map(ToString::to_string));
                    }
                }
                CssRule::LayerBlock(block) => {
                    let layer = match &block.name {
                        Some(name) => self
                            .layers
                            .child_path(context.layer, name.0.iter().map(ToString::to_string)),
                        None => self.layers.anonymous(context.layer),
                    };
                    let inner = WalkContext {
                        origin: context.origin,
                        layer,
                        nesting: context.nesting.clone(),
                        container: context.container.clone(),
                        scope: context.scope.clone(),
                        owner: context.owner,
                    };
                    self.walk(&block.rules, &inner);
                }
                CssRule::FontFace(font_face) => self.set.font_faces.push((font_face.clone(), context.origin)),
                CssRule::Property(property) => self.set.property_rules.push(property.clone()),
                CssRule::CounterStyle(rule) => self.set.counter_styles.push(rule.clone()),
                CssRule::FontPaletteValues(rule) => self.set.font_palette_values.push(rule.clone()),
                CssRule::PositionTry(rule) => self.set.position_tries.push(rule.clone()),
                CssRule::Keyframes(rule) => self.set.keyframes.push(rule.clone()),
                // Inside, `&` is the root of the scope, not a rule around it.
                CssRule::Scope(scope) => {
                    let inner = WalkContext {
                        origin: context.origin,
                        layer: context.layer,
                        nesting: None,
                        container: context.container.clone(),
                        scope: Some(Arc::new(Scope {
                            start: scope.scope_start.clone(),
                            end: scope.scope_end.clone(),
                            owner: context.owner,
                            parent: context.scope.clone(),
                        })),
                        owner: context.owner,
                    };
                    self.walk(&scope.rules, &inner);
                }
                CssRule::Page(page) => {
                    let order = self.next_order();
                    self.layer_nodes.push(context.layer);
                    let (footnote_rules, margin_rules): (Vec<_>, Vec<_>) = page
                        .rules
                        .iter()
                        .cloned()
                        .partition(crate::paged::is_footnote_area);
                    self.set.page_rules.push(PageRuleData {
                        selectors: page.selectors.iter().map(crate::PageSelector::of).collect(),
                        declarations: page.declarations.clone(),
                        margin_rules,
                        footnote_rules: footnote_rules.into_iter().map(|rule| rule.declarations).collect(),
                        origin: context.origin,
                        layer: 0,
                        order,
                    });
                }
                _ => {}
            }
        }
    }

    fn style_rule(&mut self, style: &StyleRule<'static>, context: &WalkContext) {
        let declarations = Arc::new(style.declarations.clone());
        let has_declarations =
            !declarations.declarations.is_empty() || !declarations.important_declarations.is_empty();
        if has_declarations {
            for selector in &style.selectors.0 {
                self.add(
                    selector.clone(),
                    &declarations,
                    context,
                    context.nesting.clone(),
                    style.loc.line,
                );
            }
        }
        if !style.rules.0.is_empty() {
            let nesting = Arc::new(Nesting {
                selectors: style.selectors.clone(),
                parent: context.nesting.clone(),
            });
            let inner = WalkContext {
                origin: context.origin,
                layer: context.layer,
                nesting: Some(nesting),
                container: context.container.clone(),
                scope: context.scope.clone(),
                owner: context.owner,
            };
            self.walk(&style.rules, &inner);
        }
    }

    fn add(
        &mut self,
        selector: Selector,
        declarations: &Arc<DeclarationBlock>,
        context: &WalkContext,
        nesting: Option<Arc<Nesting>>,
        line: u32,
    ) {
        if misplaced_pseudo_element(&selector) {
            return;
        }
        let mut weight = specificity(&selector);
        if let Some(parent) = &nesting
            && contains_nesting(&selector)
        {
            weight += parent.selectors.0.iter().map(specificity).max().unwrap_or(0);
        }
        let pseudo = PseudoElementKind::of_selector(&selector);
        let index = u32::try_from(self.set.rules.len()).expect("fewer than u32::MAX rules");
        match pseudo {
            Some(_) => self.set.pseudo.push(index),
            None => match index_key(&selector) {
                IndexKey::Id(id) => self.set.by_id.entry(id).or_default().push(index),
                IndexKey::Class(class) => self.set.by_class.entry(class).or_default().push(index),
                IndexKey::Tag(tag) => self.set.by_tag.entry(tag).or_default().push(index),
                IndexKey::Universal => self.set.universal.push(index),
            },
        }
        let order = self.next_order();
        self.layer_nodes.push(context.layer);
        self.set.rules.push(Rule {
            selector,
            specificity: weight,
            declarations: Arc::clone(declarations),
            origin: context.origin,
            layer: 0,
            order,
            nesting,
            pseudo,
            container: context.container.clone(),
            scope: context.scope.clone(),
            line: line + 1,
        });
    }

    fn next_order(&self) -> u32 {
        u32::try_from(self.set.rules.len() + self.set.page_rules.len()).expect("fewer than u32::MAX rules")
    }

    fn finish(mut self) -> RuleSet {
        let ranks = self.layers.ranks();
        let mut nodes = self.layer_nodes.into_iter();
        // Rules and page rules were numbered in one sequence; `order` tells them apart.
        let mut rule_iter = self.set.rules.iter_mut().peekable();
        let mut page_iter = self.set.page_rules.iter_mut().peekable();
        loop {
            let take_rule = match (rule_iter.peek(), page_iter.peek()) {
                (Some(rule), Some(page)) => rule.order < page.order,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            let node = nodes.next().expect("one layer node per rule");
            if take_rule {
                rule_iter.next().expect("peeked").layer = ranks[node];
            } else {
                page_iter.next().expect("peeked").layer = ranks[node];
            }
        }
        self.set
    }
}

enum IndexKey {
    Id(String),
    Class(String),
    Tag(String),
    Universal,
}

/// The most selective simple selector of the rightmost compound.
fn index_key(selector: &Selector) -> IndexKey {
    let mut key = IndexKey::Universal;
    for component in selector.iter_raw_match_order() {
        match component {
            Component::Combinator(_) => break,
            Component::ID(id) => return IndexKey::Id(id.0.to_string()),
            Component::Class(class) => {
                if !matches!(key, IndexKey::Class(_)) {
                    key = IndexKey::Class(class.0.to_string());
                }
            }
            Component::LocalName(name) => {
                if matches!(key, IndexKey::Universal) {
                    key = IndexKey::Tag(name.lower_name.0.to_string());
                }
            }
            _ => {}
        }
    }
    key
}

/// Whether a selector has a pseudo-element anywhere but at its end, which makes it invalid
/// (Selectors 4, §3.6); lightningcss accepts some such selectors (`li::foo p`).
fn misplaced_pseudo_element(selector: &Selector) -> bool {
    selector.has_pseudo_element()
        && !selector
            .iter()
            .any(|component| matches!(component, Component::PseudoElement(_)))
}

fn contains_nesting(selector: &Selector) -> bool {
    selector
        .iter_raw_match_order()
        .any(|component| matches!(component, Component::Nesting))
}

/// The specificity of a selector (Selectors 4, §17), where `:has()` counts as its most specific
/// argument, as `:is()` and `:not()` do, which the specificity that lightningcss computes leaves
/// out.
fn specificity(selector: &Selector) -> u32 {
    selector.specificity() + has_specificity(selector)
}

/// What `:has()` adds to the specificity of `selector`, also inside `:is()`, `:not()` and
/// `:nth-child(… of …)`, whose most specific argument may then be another.
fn has_specificity(selector: &Selector) -> u32 {
    /// How much more the most specific of `list` is with what `:has()` adds.
    fn raise(list: &[Selector]) -> u32 {
        let plain = list.iter().map(Selector::specificity).max().unwrap_or(0);
        let full = list.iter().map(specificity).max().unwrap_or(0);
        full.saturating_sub(plain)
    }
    selector
        .iter_raw_match_order()
        .map(|component| match component {
            Component::Has(list) => list.iter().map(specificity).max().unwrap_or(0),
            Component::Is(list) | Component::Negation(list) | Component::Any(_, list) => raise(list),
            Component::NthOf(nth) => raise(nth.selectors()),
            _ => 0,
        })
        .sum()
}

/// Whether Fagus supports a condition of `@supports` (as `supports()` in `if()` asks it,
/// `display: grid` or `(display: grid) and selector(a > b)`).
pub fn supports(css: &str) -> bool {
    use lightningcss::traits::Parse;
    let parse = |css: &str| {
        crate::guarded(|| {
            SupportsCondition::parse_string(css)
                .ok()
                .map(|condition| supports_condition(&condition))
        })
    };
    parse(css)
        .flatten()
        .or_else(|| parse(&format!("({css})")).flatten())
        .unwrap_or(false)
}

/// Whether Fagus supports a `@supports` condition.
fn supports_condition(condition: &SupportsCondition<'_>) -> bool {
    match condition {
        SupportsCondition::Not(inner) => !supports_condition(inner),
        SupportsCondition::And(conditions) => conditions.iter().all(supports_condition),
        SupportsCondition::Or(conditions) => conditions.iter().any(supports_condition),
        SupportsCondition::Declaration { property_id, value } => {
            extension_supported(property_id.name(), value).unwrap_or_else(|| {
                // Values with `var()` or with functions resolved for each element are taken.
                crate::guarded(|| {
                    match LightningProperty::parse_string(
                        property_id.clone(),
                        value,
                        ParserOptions::default(),
                    ) {
                        Ok(LightningProperty::Unparsed(unparsed)) => {
                            crate::has_computed_values(&unparsed.value)
                                || crate::is_anchored(&unparsed)
                                || crate::flow_position(&unparsed).is_some()
                                || crate::is_calc_size(&unparsed)
                                || crate::background_clip::is_border_area(&unparsed)
                                || (matches!(
                                    unparsed.property_id,
                                    lightningcss::properties::PropertyId::Position
                                ) && crate::paged::running_name(&unparsed.value).is_some())
                        }
                        Ok(
                            LightningProperty::Custom(_)
                            | LightningProperty::TextAlign(
                                lightningcss::properties::text::TextAlign::JustifyAll,
                            ),
                        )
                        | Err(_) => false,
                        Ok(_) => true,
                    }
                })
                .unwrap_or(false)
            })
        }
        SupportsCondition::Selector(selector) => crate::supports::selector(selector),
        SupportsCondition::Unknown(feature) => crate::supports::feature(feature),
    }
}

/// Whether a value of `-fagus-layer` is valid: `none`, or a string with `screen` or `print`
/// after it or not.
fn layer_value(value: &str) -> bool {
    if value == "none" {
        return true;
    }
    let Some(quote) = value.chars().next().filter(|quote| matches!(quote, '"' | '\'')) else {
        return false;
    };
    let Some(end) = value[1..].find(quote) else {
        return false;
    };
    let (name, rest) = (&value[1..=end], value[end + 2..].trim());
    !name.trim().is_empty() && matches!(rest, "" | "screen" | "print")
}

/// Whether Fagus supports a declaration of one of its own extensions (`-fagus-`), so that a
/// stylesheet can tell Fagus from browsers; `None` for other properties.
fn extension_supported(name: &str, value: &str) -> Option<bool> {
    let name = name.to_ascii_lowercase();
    let value = value.trim().to_ascii_lowercase();
    match name.as_str() {
        "-fagus-repeat" => Some(matches!(value.as_str(), "auto" | "at-break")),
        "-fagus-page-height" => Some(matches!(value.as_str(), "auto" | "fit-content")),
        "-fagus-layer" => Some(layer_value(&value)),
        _ if name.starts_with("-fagus-") => Some(false),
        // Properties lightningcss does not know and Fagus parses itself: the keywords of
        // `text-wrap` (as in `@supports (text-wrap: pretty)`), any value of the others.
        "text-wrap" => Some(keywords_in(
            &value,
            &["wrap", "nowrap", "balance", "pretty", "stable", "auto"],
        )),
        "text-wrap-style" => Some(keywords_in(&value, &["auto", "balance", "pretty", "stable"])),
        "text-wrap-mode" => Some(keywords_in(&value, &["wrap", "nowrap"])),
        "hanging-punctuation" => {
            Some(crate::hanging::HangingPunctuation::parse(value.split_ascii_whitespace()).is_some())
        }
        "float" => Some(matches!(
            value.as_str(),
            "left" | "right" | "none" | "inline-start" | "inline-end" | "footnote"
        )),
        "footnote-display" => Some(matches!(value.as_str(), "block" | "inline" | "compact")),
        "footnote-policy" => Some(matches!(value.as_str(), "auto" | "line" | "block")),
        "float-reference" => Some(matches!(value.as_str(), "inline" | "region" | "page" | "column")),
        "scrollbar-gutter" => Some(matches!(
            value.split_ascii_whitespace().collect::<Vec<_>>().as_slice(),
            ["auto" | "stable"] | ["stable", "both-edges"] | ["both-edges", "stable"]
        )),
        _ if crate::properties::classify(&name) == crate::properties::PropertySupport::Known => {
            Some(!value.is_empty())
        }
        _ => None,
    }
}

/// Whether a value is one or more of `keywords`, each at most once.
fn keywords_in(value: &str, keywords: &[&str]) -> bool {
    let words: Vec<&str> = value.split_ascii_whitespace().collect();
    !words.is_empty()
        && words.iter().all(|word| keywords.contains(word))
        && words
            .iter()
            .enumerate()
            .all(|(index, word)| !words[..index].contains(word))
}

/// The tree of cascade layers, in order of first appearance.
#[derive(Debug)]
struct LayerTree {
    /// Children of each node: (name, node index), in order of first appearance.
    children: Vec<Vec<(String, usize)>>,
    anonymous_count: usize,
}

impl Default for LayerTree {
    fn default() -> Self {
        Self {
            children: vec![Vec::new()],
            anonymous_count: 0,
        }
    }
}

impl LayerTree {
    fn child(&mut self, parent: usize, name: String) -> usize {
        if let Some(&(_, node)) = self.children[parent]
            .iter()
            .find(|(existing, _)| *existing == name)
        {
            return node;
        }
        let node = self.children.len();
        self.children.push(Vec::new());
        self.children[parent].push((name, node));
        node
    }

    fn child_path(&mut self, parent: usize, path: impl Iterator<Item = String>) -> usize {
        path.fold(parent, |node, name| self.child(node, name))
    }

    fn anonymous(&mut self, parent: usize) -> usize {
        self.anonymous_count += 1;
        let name = format!("\u{0}anonymous {}", self.anonymous_count);
        self.child(parent, name)
    }

    /// Rank of every node: sublayers rank below the styles directly in their parent layer, and
    /// the root (unlayered styles) ranks highest.
    fn ranks(&self) -> Vec<u32> {
        let mut ranks = vec![0; self.children.len()];
        let mut next = 0;
        self.rank(0, &mut ranks, &mut next);
        ranks
    }

    fn rank(&self, node: usize, ranks: &mut [u32], next: &mut u32) {
        for &(_, child) in &self.children[node] {
            self.rank(child, ranks, next);
        }
        ranks[node] = *next;
        *next += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_rules_for_the_screen_only() {
        let screen = |css: &str| {
            let sheet = Stylesheet::parse(css, Origin::Author, "test");
            RuleSet::new(&[&sheet], &MediaEnvironment::default()).has_screen_rules()
        };
        assert!(screen("@media screen { p { height: 10px } }"));
        assert!(screen("@media not print { p { height: 10px } }"));
        assert!(!screen("@media print { p { height: 10px } }"));
        assert!(!screen("@media screen, print { p { height: 10px } }"));
        assert!(!screen("@media screen { }"));
        assert!(!screen("p { height: 10px }"));
    }

    fn winner(css: &str, html: &str, id: &str) -> u32 {
        let document = Document::parse_html(html);
        let sheet = Stylesheet::parse(css, Origin::Author, "test");
        assert!(sheet.diagnostics().is_empty(), "{:?}", sheet.diagnostics());
        let set = RuleSet::new(&[&sheet], &MediaEnvironment::default());
        let element = document
            .descendants(NodeId::DOCUMENT)
            .find(|&node| {
                document
                    .element(node)
                    .is_some_and(|data| data.id.as_deref() == Some(id))
            })
            .expect("element");
        let rules = set.matching(&document, element, None);
        rules
            .iter()
            .max_by_key(|rule| rule.precedence(false))
            .map(|rule| rule.line)
            .expect("a matching rule")
    }

    #[test]
    fn later_layers_and_unlayered_styles_win() {
        let css = "@layer base, utilities;
@layer utilities { #x { color: green } }
@layer base { #x { color: red } }";
        assert_eq!(winner(css, "<p id=x>", "x"), 2);

        let css = "#x { color: green }
@layer a { #x { color: red } }";
        assert_eq!(winner(css, "<p id=x>", "x"), 1);
    }

    #[test]
    fn has_counts_as_its_most_specific_argument() {
        // `:has(input:checked)` weighs as much as `input:checked`: it beats `:nth-child()`
        // that comes first, as a selected row of a striped table is highlighted in Chrome.
        let html = "<table><tr><td></td></tr><tr><td id=x><input type=checkbox checked></td></tr></table>";
        let css = "tr:nth-child(even) td { color: red }
tr:has(input:checked) td#x { color: green }
tr:has(input:checked) td { color: green }";
        assert_eq!(winner(&css.replace("td#x", "th"), html, "x"), 3);
        // `:has(#id)` beats a class; inside `:is()` too, where its argument counts.
        let html = "<div><i id=x class=c></i><b id=q></b></div>";
        assert_eq!(
            winner(
                "div:has(#q) i { color: green }\ndiv i.c { color: red }",
                html,
                "x"
            ),
            1
        );
        assert_eq!(
            winner(
                "div i.c { color: red }\n:is(div:has(#q), p) i { color: green }",
                html,
                "x"
            ),
            2
        );
        assert_eq!(
            winner(
                "div i.c.c { color: red }\n:is(div:has(.c), p) i { color: green }",
                html,
                "x"
            ),
            1
        );
    }

    #[test]
    fn styles_directly_in_a_layer_beat_its_sublayers() {
        let css = "@layer a { #x { color: green } @layer b { #x { color: red } } }";
        assert_eq!(winner(css, "<p id=x>", "x"), 1);
    }

    #[test]
    fn drops_selectors_with_misplaced_pseudo_elements() {
        // A pseudo-element may only end a selector; tests run with debug assertions, which
        // parcel_selectors trips over for such selectors.
        let sheet = Stylesheet::parse("li::unknown p { color: red }", Origin::Author, "test");
        let set = RuleSet::new(&[&sheet], &MediaEnvironment::default());
        let document = Document::parse_html("<ul><li><p>x</p></li></ul>");
        let paragraph = document.find("p").expect("p");
        assert!(set.matching(&document, paragraph, None).is_empty());
    }

    #[test]
    fn resolves_nesting_and_media() {
        let css = ".card { color: red;
  &.ok { color: green } }
@media screen { .card { color: blue } }";
        assert_eq!(winner(css, "<div id=x class='card ok'>", "x"), 2);

        let css = ".card { color: red; .title { color: green } }";
        assert_eq!(
            winner(css, "<div class=card><p id=x class=title></p></div>", "x"),
            1
        );
    }

    #[test]
    fn evaluates_supports() {
        let css = "#x { color: red }
@supports (color: oklch(0.5 0.1 100)) { #x { color: green } }
@supports (color: nonsense) { #x { color: blue } }";
        assert_eq!(winner(css, "<p id=x>", "x"), 2);

        // Fagus's own extensions, which browsers do not know.
        let css = "#x { color: red }
@supports (-fagus-repeat: at-break) { #x { color: green } }
@supports (-fagus-repeat: sometimes) or (-fagus-unknown: auto) { #x { color: blue } }";
        assert_eq!(winner(css, "<p id=x>", "x"), 2);
        let css = "#x { color: red }
@supports (-fagus-page-height: fit-content) { #x { color: green } }
@supports (-fagus-page-height: 100mm) { #x { color: blue } }";
        assert_eq!(winner(css, "<p id=x>", "x"), 2);
        let css = "#x { color: red }
@supports (-fagus-layer: \"Notes\" screen) and (-fagus-layer: none) { #x { color: green } }
@supports (-fagus-layer: \"Notes\" paper) or (-fagus-layer: Notes) or (-fagus-layer: \"\") { #x { color: blue } }";
        assert_eq!(winner(css, "<p id=x>", "x"), 2);

        // Properties that Fagus parses itself, with the values it knows.
        let css = "#x { color: red }
@supports (text-wrap: pretty) and (break-inside: avoid) { #x { color: green } }
@supports (text-wrap: sideways) or (text-wrap-mode: balance) { #x { color: blue } }";
        assert_eq!(winner(css, "<p id=x>", "x"), 2);

        // Footnotes are (CSS Generated Content for Paged Media 3), unlike in Chrome.
        let css = "#x { color: red }
@supports (float: inline-start) { #x { color: green } }
@supports (float: footnote) and (footnote-display: block) and (footnote-policy: line) { #x { color: blue } }
@supports (footnote-display: list) or (float-reference: side) { #x { color: yellow } }";
        assert_eq!(winner(css, "<p id=x>", "x"), 3);
        let css = "#x { color: red }
@supports (float-reference: page) and (float-reference: region) and (float-reference: column) { #x { color: green } }";
        assert_eq!(winner(css, "<p id=x>", "x"), 2);
        // `scrollbar-gutter` takes its values of CSS Overflow 4.
        let css = "#x { color: red }
@supports (scrollbar-gutter: both-edges stable) and (scrollbar-gutter: auto) { #x { color: green } }
@supports (scrollbar-gutter: both-edges) or (scrollbar-gutter: stable stable) { #x { color: blue } }";
        assert_eq!(winner(css, "<p id=x>", "x"), 2);
        // Hanging punctuation is, with the values of CSS Text 3 (Chrome does not support it).
        let css = "#x { color: red }
@supports (hanging-punctuation: first allow-end last) { #x { color: green } }
@supports (hanging-punctuation: force-end allow-end) { #x { color: blue } }";
        assert_eq!(winner(css, "<p id=x>", "x"), 2);
        // Running elements are (CSS Generated Content for Paged Media 3), with one name.
        let css = "#x { color: red }
@supports (position: running(header)) and (position: RUNNING(x)) { #x { color: green } }
@supports (position: running()) or (position: running(1)) or (position: running(a b)) or (position: running(a) fixed) { #x { color: blue } }";
        assert_eq!(winner(css, "<p id=x>", "x"), 2);
        assert!(supports("position: running(header)"));
        assert!(!supports("display: running(header)"));
    }

    #[test]
    fn collects_page_and_font_face_rules() {
        let sheet = Stylesheet::parse(
            "@page { size: A4; margin: 20mm; @bottom-right { content: counter(page) } }
             @font-face { font-family: Inter; src: url(inter.ttf) }",
            Origin::Author,
            "test",
        );
        let set = RuleSet::new(&[&sheet], &MediaEnvironment::default());
        assert_eq!(set.page_rules().len(), 1);
        assert_eq!(set.page_rules()[0].margin_rules.len(), 1);
        assert_eq!(set.font_faces().len(), 1);

        // `@footnote` rules are apart from the margin rules.
        let sheet = Stylesheet::parse(
            "@page { @footnote { border-top: 1px solid } @bottom-center { content: 'x' } }",
            Origin::Author,
            "test",
        );
        let set = RuleSet::new(&[&sheet], &MediaEnvironment::default());
        assert_eq!(sheet.diagnostics(), []);
        let rule = &set.page_rules()[0];
        assert_eq!(rule.margin_rules.len(), 1);
        assert_eq!(rule.footnote_rules.len(), 1);
        assert_eq!(rule.footnote_rules[0].declarations.len(), 2);
    }
}

use crate::syn_wrappers::{Syn, SynKind};
use proc_macro2::Span;
use std::collections::{HashMap, HashSet};
use std::ptr::NonNull;
use std::rc::Rc;
use syn::spanned::Spanned;
use syn::visit::Visit;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct NodeId(usize);

impl From<usize> for NodeId {
    fn from(n: usize) -> Self {
        NodeId(n)
    }
}

#[derive(Clone, PartialOrd, Ord, PartialEq, Eq, Copy, Debug)]
pub struct Location {
    /// 1-based
    pub line: usize,
    /// 0-based
    pub column: usize,
}

impl Location {
    fn next(&self, line_info: &[usize]) -> Option<Location> {
        let max_line = line_info.len();
        let current_line_length = line_info[self.line - 1];
        if self.column == current_line_length {
            if self.line == max_line {
                None
            } else {
                Some(Location {
                    line: self.line + 1,
                    column: 0,
                })
            }
        } else {
            Some(Location {
                column: self.column + 1,
                ..*self
            })
        }
    }

    fn prev(&self, line_info: &[usize]) -> Option<Location> {
        if self.column == 0 {
            if self.line == 1 {
                None
            } else {
                Some(Location {
                    line: self.line - 1,
                    column: line_info[self.line - 2],
                })
            }
        } else {
            Some(Location {
                column: self.column - 1,
                ..*self
            })
        }
    }
}

impl From<proc_macro2::LineColumn> for Location {
    fn from(line_column: proc_macro2::LineColumn) -> Self {
        Self {
            line: line_column.line,
            column: line_column.column,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Ptr {
    owner: Rc<syn::File>,
    ptr: NonNull<()>,
    kind: SynKind,
}

impl Ptr {
    fn new(owner: Rc<syn::File>, node: Syn) -> Self {
        Ptr {
            owner: owner.clone(),
            kind: (&node).into(),
            ptr: unsafe { NonNull::new_unchecked(node.data() as *mut _) },
        }
    }

    pub fn as_syn(&self) -> Syn {
        unsafe { Syn::from_raw(self.ptr.as_ptr() as *const _, self.kind) }
    }
}

impl std::hash::Hash for Ptr {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.ptr.hash(state)
    }
}

impl std::cmp::PartialEq<Ptr> for Ptr {
    fn eq(&self, other: &Ptr) -> bool {
        self.ptr.eq(&other.ptr)
    }
}

impl std::cmp::Eq for Ptr {}

pub type Range = (Location, Location);

pub struct IrVisitor {
    counter: usize,
    file: Rc<syn::File>,
    id_to_ptr: HashMap<NodeId, PtrData>,
    locations: HashMap<NodeId, LocationData>,
    ancestors: Vec<NodeId>,
    line_info: Vec<usize>,
}

#[derive(Debug, Clone)]
struct LocationData {
    ranges: Vec<Range>,
    blocked: Vec<Range>,
}

impl IrVisitor {
    pub fn new(file: Rc<syn::File>, line_info: Vec<usize>) -> Self {
        IrVisitor {
            counter: 1,
            id_to_ptr: Default::default(),
            ancestors: vec![],
            file,
            locations: Default::default(),
            line_info,
        }
    }

    pub fn visit(mut self) -> crate::analysis::Analyzer {
        let file = self.file.clone();
        self.visit_file(&file);

        #[cfg(feature = "dev")]
        {
            let clone: Vec<_> = self
                .locations
                .clone()
                .into_iter()
                .flat_map(|(_, data)| data.ranges.into_iter())
                .collect();

            for (i, range) in clone.iter().take(clone.len() - 1).enumerate() {
                for other in &clone[(i + 1)..] {
                    if range.1 <= other.0 || other.1 <= range.0 {
                        continue;
                    }
                    panic!("Unexpected overlap between {:?} and {:?}", range, other);
                }
            }
        }

        let mut locations: Vec<_> = self
            .locations
            .into_iter()
            .flat_map(|(id, location_data)| {
                location_data.ranges.into_iter().map(move |pos| (id, pos))
            })
            .collect();

        locations.sort_by_key(|(_, pos)| *pos);

        let locations: Vec<_> = locations
            .into_iter()
            .map(|(id, range)| (id, range))
            .collect();

        crate::analysis::Analyzer {
            locations,
            id_to_ptr: self.id_to_ptr,
        }
    }
}

pub(crate) struct PtrData {
    pub parent: NodeId,
    pub ptr: Ptr,
    pub children: HashSet<NodeId>,
}

impl IrVisitor {
    fn prepare(&mut self, node: Syn, span: Span) -> NodeId {
        let start: Location = span.start().into();
        let end: Location = span.end().into();
        self.prepare_precise(node, (start, end))
    }

    #[inline(never)]
    fn prepare_precise(&mut self, node: Syn, (start, end): Range) -> NodeId {
        self.prepare_precise_ranges(node, &[(start, end)])
    }

    fn prepare_precise_ranges(&mut self, node: Syn, ranges: &[Range]) -> NodeId {
        #[cfg(feature = "dev")]
        {
            for range in ranges {
                let (start, end) = range;
                if start == end {
                    match node {
                        Syn::File(..) => {}
                        _ => {
                            panic!("Unexpected start == end {:?}", start);
                        }
                    }
                }
            }
        }
        let ptr = Ptr::new(self.file.clone(), node);

        let id = self.counter.into();
        self.counter += 1;

        let mut data = PtrData {
            parent: NodeId(0),
            children: Default::default(),
            ptr: ptr.clone(),
        };

        let blocked = if let Some(&ancestor_id) = self.ancestors.last() {
            data.parent = ancestor_id;

            if let Some(ancestor_data) = self.id_to_ptr.get_mut(&ancestor_id) {
                ancestor_data.children.insert(id);
            }

            for range in ranges {
                self.steal_ancestor_locations(ancestor_id, *range);
            }

            self.locations
                .get(&ancestor_id)
                .map(|data| &data.blocked[..])
                .unwrap_or(&[])
        } else {
            &[]
        };

        let mut ranges: Vec<_> = ranges.iter().cloned().collect();

        for blocked_range in blocked {
            IrVisitor::recalculate_location(&self.line_info[..], *blocked_range, &mut ranges);
        }

        self.id_to_ptr.insert(id, data);
        self.locations.insert(
            id,
            LocationData {
                ranges,
                blocked: vec![],
            },
        );
        id
    }

    fn steal_ancestor_locations(&mut self, ancestor_id: NodeId, child: Range) {
        let ancestor_locations =
            if let Some(ancestor_locations) = self.locations.get_mut(&ancestor_id) {
                ancestor_locations
            } else {
                return;
            };

        let _changed = IrVisitor::recalculate_location(
            &self.line_info[..],
            child,
            &mut ancestor_locations.ranges,
        );

        #[cfg(feature = "dev")]
        {
            if _changed {
                ancestor_locations.ranges.sort();

                let mut i = 0;
                loop {
                    if i + 1 >= ancestor_locations.ranges.len() {
                        break;
                    }

                    let next = ancestor_locations.ranges[i + 1];
                    let el = &mut ancestor_locations.ranges[i];

                    if next.0 == el.1 {
                        panic!("TODO This should not be necessary");
                    // el.1 = next.1;
                    // ancestor_locations.remove(i + 1);
                    } else {
                        i += 1;
                    }
                }
            }
        }
    }

    fn recalculate_location(line_info: &[usize], child: Range, locations: &mut Vec<Range>) -> bool {
        let mut changed = false;

        let new_locations = locations.drain(..).fold(vec![], |mut acc, range| {
            let diff = range_difference(range, child, line_info);

            for interval in diff.iter().cloned().filter_map(|x| x) {
                changed = changed || (interval != range);
                acc.push(interval);
            }

            acc
        });

        *locations = new_locations;
        changed
    }
}

fn range_difference(parent: Range, child: Range, line_info: &[usize]) -> [Option<Range>; 3] {
    let mut ret = [None; 3];
    let (start, end) = child;
    let child_to_left = end < parent.0;
    let child_to_right = start > parent.1;

    if child_to_left || child_to_right {
        ret[0] = Some(parent);
        return ret;
    }

    let cuts_prefix = start > parent.0;
    let cuts_suffix = end < parent.1;

    if cuts_prefix {
        if let Some(prefix_end) = start.prev(line_info) {
            if prefix_end >= parent.0 {
                ret[1] = Some((parent.0, prefix_end));
            }
        }
    }

    if cuts_suffix {
        if let Some(suffix_start) = end.next(line_info) {
            if suffix_start <= parent.1 {
                ret[2] = Some((suffix_start, parent.1));
            }
        }
    }

    ret
}

macro_rules! visit {
    ($self:ident, $node:ident, $name:ident) => {
        let id = $self.prepare(Syn::from($node), $node.span());
        $self.ancestors.push(id);
        syn::visit::$name($self, $node);
        let _ = $self.ancestors.pop();
    };
}

impl<'ast> Visit<'ast> for IrVisitor {
    fn visit_abi(&mut self, i: &'ast syn::Abi) {
        visit![self, i, visit_abi];
    }
    fn visit_angle_bracketed_generic_arguments(
        &mut self,
        i: &'ast syn::AngleBracketedGenericArguments,
    ) {
        visit![self, i, visit_angle_bracketed_generic_arguments];
    }
    fn visit_arm(&mut self, i: &'ast syn::Arm) {
        visit![self, i, visit_arm];
    }
    fn visit_attr_style(&mut self, _i: &'ast syn::AttrStyle) {
        // SPECIAL: NO SPAN
    }
    fn visit_attribute(&mut self, i: &'ast syn::Attribute) {
        // SPECIAL: OVERLAPPING SPANS
        if let syn::AttrStyle::Outer = i.style {
            visit![self, i, visit_attribute];
            return;
        }
        let id = self.prepare(i.into(), i.span());
        if let Some(data) = self
            .ancestors
            .last()
            .cloned()
            .and_then(|ancestor_id| self.locations.get_mut(&ancestor_id))
        {
            let span = i.span();
            data.blocked.push((span.start().into(), span.end().into()))
        }
        self.ancestors.push(id);
        syn::visit::visit_attribute(self, i);
        let _ = self.ancestors.pop();
    }
    fn visit_bare_fn_arg(&mut self, i: &'ast syn::BareFnArg) {
        visit![self, i, visit_bare_fn_arg];
    }
    fn visit_bin_op(&mut self, i: &'ast syn::BinOp) {
        visit![self, i, visit_bin_op];
    }
    fn visit_binding(&mut self, i: &'ast syn::Binding) {
        visit![self, i, visit_binding];
    }
    fn visit_block(&mut self, i: &'ast syn::Block) {
        visit![self, i, visit_block];
    }
    fn visit_bound_lifetimes(&mut self, i: &'ast syn::BoundLifetimes) {
        visit![self, i, visit_bound_lifetimes];
    }
    fn visit_const_param(&mut self, i: &'ast syn::ConstParam) {
        visit![self, i, visit_const_param];
    }
    fn visit_constraint(&mut self, i: &'ast syn::Constraint) {
        visit![self, i, visit_constraint];
    }
    fn visit_data(&mut self, _i: &'ast syn::Data) {
        // SPECIAL: OMITTED
    }
    fn visit_data_enum(&mut self, _i: &'ast syn::DataEnum) {
        // SPECIAL: OMITTED
    }
    fn visit_data_struct(&mut self, _i: &'ast syn::DataStruct) {
        // SPECIAL: OMITTED
    }
    fn visit_data_union(&mut self, _i: &'ast syn::DataUnion) {
        // SPECIAL: OMITTED
    }
    fn visit_derive_input(&mut self, _i: &'ast syn::DeriveInput) {
        // SPECIAL: OMITTED
    }
    fn visit_expr(&mut self, i: &'ast syn::Expr) {
        // SPECIAL: EMPTY SPAN
        if let syn::Expr::Verbatim(_) = i {
            return;
        }
        visit![self, i, visit_expr];
    }
    fn visit_expr_array(&mut self, i: &'ast syn::ExprArray) {
        visit![self, i, visit_expr_array];
    }
    fn visit_expr_assign(&mut self, i: &'ast syn::ExprAssign) {
        visit![self, i, visit_expr_assign];
    }
    fn visit_expr_assign_op(&mut self, i: &'ast syn::ExprAssignOp) {
        visit![self, i, visit_expr_assign_op];
    }
    fn visit_expr_async(&mut self, i: &'ast syn::ExprAsync) {
        visit![self, i, visit_expr_async];
    }
    fn visit_expr_await(&mut self, i: &'ast syn::ExprAwait) {
        visit![self, i, visit_expr_await];
    }
    fn visit_expr_binary(&mut self, i: &'ast syn::ExprBinary) {
        visit![self, i, visit_expr_binary];
    }
    fn visit_expr_block(&mut self, i: &'ast syn::ExprBlock) {
        visit![self, i, visit_expr_block];
    }
    fn visit_expr_box(&mut self, i: &'ast syn::ExprBox) {
        visit![self, i, visit_expr_box];
    }
    fn visit_expr_break(&mut self, i: &'ast syn::ExprBreak) {
        visit![self, i, visit_expr_break];
    }
    fn visit_expr_call(&mut self, i: &'ast syn::ExprCall) {
        visit![self, i, visit_expr_call];
    }
    fn visit_expr_cast(&mut self, i: &'ast syn::ExprCast) {
        visit![self, i, visit_expr_cast];
    }
    fn visit_expr_closure(&mut self, i: &'ast syn::ExprClosure) {
        visit![self, i, visit_expr_closure];
    }
    fn visit_expr_continue(&mut self, i: &'ast syn::ExprContinue) {
        visit![self, i, visit_expr_continue];
    }
    fn visit_expr_field(&mut self, i: &'ast syn::ExprField) {
        visit![self, i, visit_expr_field];
    }
    fn visit_expr_for_loop(&mut self, i: &'ast syn::ExprForLoop) {
        visit![self, i, visit_expr_for_loop];
    }
    fn visit_expr_group(&mut self, i: &'ast syn::ExprGroup) {
        visit![self, i, visit_expr_group];
    }
    fn visit_expr_if(&mut self, i: &'ast syn::ExprIf) {
        visit![self, i, visit_expr_if];
    }
    fn visit_expr_index(&mut self, i: &'ast syn::ExprIndex) {
        visit![self, i, visit_expr_index];
    }
    fn visit_expr_let(&mut self, i: &'ast syn::ExprLet) {
        visit![self, i, visit_expr_let];
    }
    fn visit_expr_lit(&mut self, i: &'ast syn::ExprLit) {
        visit![self, i, visit_expr_lit];
    }
    fn visit_expr_loop(&mut self, i: &'ast syn::ExprLoop) {
        visit![self, i, visit_expr_loop];
    }
    fn visit_expr_macro(&mut self, i: &'ast syn::ExprMacro) {
        visit![self, i, visit_expr_macro];
    }
    fn visit_expr_match(&mut self, i: &'ast syn::ExprMatch) {
        visit![self, i, visit_expr_match];
    }
    fn visit_expr_method_call(&mut self, i: &'ast syn::ExprMethodCall) {
        visit![self, i, visit_expr_method_call];
    }
    fn visit_expr_paren(&mut self, i: &'ast syn::ExprParen) {
        visit![self, i, visit_expr_paren];
    }
    fn visit_expr_path(&mut self, i: &'ast syn::ExprPath) {
        visit![self, i, visit_expr_path];
    }
    fn visit_expr_range(&mut self, i: &'ast syn::ExprRange) {
        visit![self, i, visit_expr_range];
    }
    fn visit_expr_reference(&mut self, i: &'ast syn::ExprReference) {
        visit![self, i, visit_expr_reference];
    }
    fn visit_expr_repeat(&mut self, i: &'ast syn::ExprRepeat) {
        visit![self, i, visit_expr_repeat];
    }
    fn visit_expr_return(&mut self, i: &'ast syn::ExprReturn) {
        visit![self, i, visit_expr_return];
    }
    fn visit_expr_struct(&mut self, i: &'ast syn::ExprStruct) {
        visit![self, i, visit_expr_struct];
    }
    fn visit_expr_try(&mut self, i: &'ast syn::ExprTry) {
        visit![self, i, visit_expr_try];
    }
    fn visit_expr_try_block(&mut self, i: &'ast syn::ExprTryBlock) {
        visit![self, i, visit_expr_try_block];
    }
    fn visit_expr_tuple(&mut self, i: &'ast syn::ExprTuple) {
        visit![self, i, visit_expr_tuple];
    }
    fn visit_expr_type(&mut self, i: &'ast syn::ExprType) {
        visit![self, i, visit_expr_type];
    }
    fn visit_expr_unary(&mut self, i: &'ast syn::ExprUnary) {
        visit![self, i, visit_expr_unary];
    }
    fn visit_expr_unsafe(&mut self, i: &'ast syn::ExprUnsafe) {
        visit![self, i, visit_expr_unsafe];
    }
    fn visit_expr_while(&mut self, i: &'ast syn::ExprWhile) {
        visit![self, i, visit_expr_while];
    }
    fn visit_expr_yield(&mut self, i: &'ast syn::ExprYield) {
        visit![self, i, visit_expr_yield];
    }
    fn visit_field(&mut self, i: &'ast syn::Field) {
        visit![self, i, visit_field];
    }
    fn visit_field_pat(&mut self, i: &'ast syn::FieldPat) {
        // SPECIAL: SPANS OVERLAP
        if i.colon_token.is_some() {
            visit![self, i, visit_field_pat];
            return;
        }
        let id = self.prepare(i.into(), i.span());
        self.ancestors.push(id);
        for attr in &i.attrs {
            self.visit_attribute(attr);
        }
        self.visit_member(&i.member);
        let _ = self.ancestors.pop();
    }
    fn visit_field_value(&mut self, i: &'ast syn::FieldValue) {
        // SPECIAL: SPANS OVERLAP
        if i.colon_token.is_some() {
            visit![self, i, visit_field_value];
            return;
        }
        let id = self.prepare(i.into(), i.span());
        self.ancestors.push(id);
        for attr in &i.attrs {
            self.visit_attribute(attr);
        }
        self.visit_member(&i.member);
        let _ = self.ancestors.pop();
    }
    fn visit_fields(&mut self, i: &'ast syn::Fields) {
        // SPECIAL: EMPTY SPAN
        if let syn::Fields::Unit = i {
            return;
        }
        visit![self, i, visit_fields];
    }
    fn visit_fields_named(&mut self, i: &'ast syn::FieldsNamed) {
        visit![self, i, visit_fields_named];
    }
    fn visit_fields_unnamed(&mut self, i: &'ast syn::FieldsUnnamed) {
        visit![self, i, visit_fields_unnamed];
    }
    fn visit_file(&mut self, i: &'ast syn::File) {
        visit![self, i, visit_file];
    }
    fn visit_fn_arg(&mut self, i: &'ast syn::FnArg) {
        visit![self, i, visit_fn_arg];
    }
    fn visit_foreign_item(&mut self, i: &'ast syn::ForeignItem) {
        visit![self, i, visit_foreign_item];
    }
    fn visit_foreign_item_fn(&mut self, i: &'ast syn::ForeignItemFn) {
        visit![self, i, visit_foreign_item_fn];
    }
    fn visit_foreign_item_macro(&mut self, i: &'ast syn::ForeignItemMacro) {
        visit![self, i, visit_foreign_item_macro];
    }
    fn visit_foreign_item_static(&mut self, i: &'ast syn::ForeignItemStatic) {
        visit![self, i, visit_foreign_item_static];
    }
    fn visit_foreign_item_type(&mut self, i: &'ast syn::ForeignItemType) {
        visit![self, i, visit_foreign_item_type];
    }
    fn visit_generic_argument(&mut self, i: &'ast syn::GenericArgument) {
        visit![self, i, visit_generic_argument];
    }
    fn visit_generic_method_argument(&mut self, i: &'ast syn::GenericMethodArgument) {
        visit![self, i, visit_generic_method_argument];
    }
    fn visit_generic_param(&mut self, i: &'ast syn::GenericParam) {
        visit![self, i, visit_generic_param];
    }
    fn visit_generics(&mut self, i: &'ast syn::Generics) {
        // SPECIAL: EMPTY SPAN, DISJOINT SPANS

        let range: Range = (i.span().start().into(), i.span().end().into());
        let where_range = i
            .where_clause
            .as_ref()
            .map(|where_clause| {
                let span = where_clause.span();
                (span.start().into(), span.end().into())
            })
            .unwrap_or(range);

        let full = [range, where_range];
        let single = [range];
        let where_single = [where_range];

        let ranges: &[_] = match (i.lt_token, &i.where_clause) {
            (Some(..), Some(_)) => &full,
            (Some(..), None) => &single,
            (None, Some(_)) => &where_single,
            _ => &[],
        };

        let id = self.prepare_precise_ranges(i.into(), ranges);
        self.ancestors.push(id);
        syn::visit::visit_generics(self, i);
        let _ = self.ancestors.pop();
    }
    fn visit_ident(&mut self, i: &'ast proc_macro2::Ident) {
        // SPECIAL: DO NOT VISIT
        let _ = self.prepare(i.into(), i.span());
    }
    fn visit_impl_item(&mut self, i: &'ast syn::ImplItem) {
        visit![self, i, visit_impl_item];
    }
    fn visit_impl_item_const(&mut self, i: &'ast syn::ImplItemConst) {
        visit![self, i, visit_impl_item_const];
    }
    fn visit_impl_item_macro(&mut self, i: &'ast syn::ImplItemMacro) {
        visit![self, i, visit_impl_item_macro];
    }
    fn visit_impl_item_method(&mut self, i: &'ast syn::ImplItemMethod) {
        visit![self, i, visit_impl_item_method];
    }
    fn visit_impl_item_type(&mut self, i: &'ast syn::ImplItemType) {
        visit![self, i, visit_impl_item_type];
    }
    fn visit_index(&mut self, i: &'ast syn::Index) {
        visit![self, i, visit_index];
    }
    fn visit_item(&mut self, i: &'ast syn::Item) {
        visit![self, i, visit_item];
    }
    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        visit![self, i, visit_item_const];
    }
    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        visit![self, i, visit_item_enum];
    }
    fn visit_item_extern_crate(&mut self, i: &'ast syn::ItemExternCrate) {
        visit![self, i, visit_item_extern_crate];
    }
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        visit![self, i, visit_item_fn];
    }
    fn visit_item_foreign_mod(&mut self, i: &'ast syn::ItemForeignMod) {
        visit![self, i, visit_item_foreign_mod];
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        visit![self, i, visit_item_impl];
    }
    fn visit_item_macro(&mut self, i: &'ast syn::ItemMacro) {
        visit![self, i, visit_item_macro];
    }
    fn visit_item_macro2(&mut self, i: &'ast syn::ItemMacro2) {
        visit![self, i, visit_item_macro2];
    }
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        visit![self, i, visit_item_mod];
    }
    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        visit![self, i, visit_item_static];
    }
    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        visit![self, i, visit_item_struct];
    }
    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        visit![self, i, visit_item_trait];
    }
    fn visit_item_trait_alias(&mut self, i: &'ast syn::ItemTraitAlias) {
        visit![self, i, visit_item_trait_alias];
    }
    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        visit![self, i, visit_item_type];
    }
    fn visit_item_union(&mut self, i: &'ast syn::ItemUnion) {
        visit![self, i, visit_item_union];
    }
    fn visit_item_use(&mut self, i: &'ast syn::ItemUse) {
        visit![self, i, visit_item_use];
    }
    fn visit_label(&mut self, i: &'ast syn::Label) {
        visit![self, i, visit_label];
    }
    fn visit_lifetime(&mut self, i: &'ast syn::Lifetime) {
        visit![self, i, visit_lifetime];
    }
    fn visit_lifetime_def(&mut self, i: &'ast syn::LifetimeDef) {
        visit![self, i, visit_lifetime_def];
    }
    fn visit_lit(&mut self, i: &'ast syn::Lit) {
        visit![self, i, visit_lit];
    }
    fn visit_lit_bool(&mut self, i: &'ast syn::LitBool) {
        visit![self, i, visit_lit_bool];
    }
    fn visit_lit_byte(&mut self, i: &'ast syn::LitByte) {
        visit![self, i, visit_lit_byte];
    }
    fn visit_lit_byte_str(&mut self, i: &'ast syn::LitByteStr) {
        visit![self, i, visit_lit_byte_str];
    }
    fn visit_lit_char(&mut self, i: &'ast syn::LitChar) {
        visit![self, i, visit_lit_char];
    }
    fn visit_lit_float(&mut self, i: &'ast syn::LitFloat) {
        visit![self, i, visit_lit_float];
    }
    fn visit_lit_int(&mut self, i: &'ast syn::LitInt) {
        visit![self, i, visit_lit_int];
    }
    fn visit_lit_str(&mut self, i: &'ast syn::LitStr) {
        visit![self, i, visit_lit_str];
    }
    fn visit_local(&mut self, i: &'ast syn::Local) {
        visit![self, i, visit_local];
    }
    fn visit_macro(&mut self, i: &'ast syn::Macro) {
        // SPECIAL: SPAN OVERLAP
        let item_parent_ident = self
            .ancestors
            .last()
            .and_then(|id| self.id_to_ptr.get(id))
            .and_then(|data| {
                if let Syn::ItemMacro(i) = data.ptr.as_syn() {
                    Some(i)
                } else {
                    None
                }
            })
            .and_then(|item_macro| item_macro.ident.as_ref());

        let ident_end = if let Some(ident) = item_parent_ident {
            ident.span().end()
        } else {
            visit![self, i, visit_macro];
            return;
        };

        let ranges = [
            (i.span().start().into(), i.bang_token.span().end().into()),
            (ident_end.into(), i.span().end().into()),
        ];

        let id = self.prepare_precise_ranges(i.into(), &ranges);
        self.ancestors.push(id);
        syn::visit::visit_macro(self, i);
        let _ = self.ancestors.pop();
    }
    fn visit_macro_delimiter(&mut self, _i: &'ast syn::MacroDelimiter) {
        // SPECIAL: NO SPAN
    }
    fn visit_member(&mut self, i: &'ast syn::Member) {
        visit![self, i, visit_member];
    }
    fn visit_meta(&mut self, _i: &'ast syn::Meta) {
        //SPECIAL: OMITTED
    }
    fn visit_meta_list(&mut self, _i: &'ast syn::MetaList) {
        //SPECIAL: OMITTED
    }
    fn visit_meta_name_value(&mut self, _i: &'ast syn::MetaNameValue) {
        //SPECIAL: OMITTED
    }
    fn visit_method_turbofish(&mut self, i: &'ast syn::MethodTurbofish) {
        visit![self, i, visit_method_turbofish];
    }
    fn visit_nested_meta(&mut self, _i: &'ast syn::NestedMeta) {
        //SPECIAL: OMITTED
    }
    fn visit_parenthesized_generic_arguments(
        &mut self,
        i: &'ast syn::ParenthesizedGenericArguments,
    ) {
        visit![self, i, visit_parenthesized_generic_arguments];
    }
    fn visit_pat(&mut self, i: &'ast syn::Pat) {
        visit![self, i, visit_pat];
    }
    fn visit_pat_box(&mut self, i: &'ast syn::PatBox) {
        visit![self, i, visit_pat_box];
    }
    fn visit_pat_ident(&mut self, i: &'ast syn::PatIdent) {
        visit![self, i, visit_pat_ident];
    }
    fn visit_pat_lit(&mut self, i: &'ast syn::PatLit) {
        visit![self, i, visit_pat_lit];
    }
    fn visit_pat_macro(&mut self, i: &'ast syn::PatMacro) {
        visit![self, i, visit_pat_macro];
    }
    fn visit_pat_or(&mut self, i: &'ast syn::PatOr) {
        visit![self, i, visit_pat_or];
    }
    fn visit_pat_path(&mut self, i: &'ast syn::PatPath) {
        visit![self, i, visit_pat_path];
    }
    fn visit_pat_range(&mut self, i: &'ast syn::PatRange) {
        visit![self, i, visit_pat_range];
    }
    fn visit_pat_reference(&mut self, i: &'ast syn::PatReference) {
        visit![self, i, visit_pat_reference];
    }
    fn visit_pat_rest(&mut self, i: &'ast syn::PatRest) {
        visit![self, i, visit_pat_rest];
    }
    fn visit_pat_slice(&mut self, i: &'ast syn::PatSlice) {
        visit![self, i, visit_pat_slice];
    }
    fn visit_pat_struct(&mut self, i: &'ast syn::PatStruct) {
        visit![self, i, visit_pat_struct];
    }
    fn visit_pat_tuple(&mut self, i: &'ast syn::PatTuple) {
        visit![self, i, visit_pat_tuple];
    }
    fn visit_pat_tuple_struct(&mut self, i: &'ast syn::PatTupleStruct) {
        visit![self, i, visit_pat_tuple_struct];
    }
    fn visit_pat_type(&mut self, i: &'ast syn::PatType) {
        visit![self, i, visit_pat_type];
    }
    fn visit_pat_wild(&mut self, i: &'ast syn::PatWild) {
        visit![self, i, visit_pat_wild];
    }
    fn visit_path(&mut self, i: &'ast syn::Path) {
        visit![self, i, visit_path];
    }
    fn visit_path_arguments(&mut self, i: &'ast syn::PathArguments) {
        // SPECIAL: EMPTY SPAN
        if let syn::PathArguments::None = i {
            return;
        }
        visit![self, i, visit_path_arguments];
    }
    fn visit_path_segment(&mut self, i: &'ast syn::PathSegment) {
        visit![self, i, visit_path_segment];
    }
    fn visit_predicate_eq(&mut self, i: &'ast syn::PredicateEq) {
        visit![self, i, visit_predicate_eq];
    }
    fn visit_predicate_lifetime(&mut self, i: &'ast syn::PredicateLifetime) {
        visit![self, i, visit_predicate_lifetime];
    }
    fn visit_predicate_type(&mut self, i: &'ast syn::PredicateType) {
        visit![self, i, visit_predicate_type];
    }
    fn visit_qself(&mut self, i: &'ast syn::QSelf) {
        // SPECIAL: NO SPAN
        let end = if let Some(as_token) = i.as_token {
            let span = as_token.span();
            span.end()
        } else {
            i.gt_token.span().end()
        };

        let id = self.prepare_precise(i.into(), (i.lt_token.span().start().into(), end.into()));

        self.ancestors.push(id);
        syn::visit::visit_qself(self, i);
        let _ = self.ancestors.pop();
    }
    fn visit_range_limits(&mut self, _i: &'ast syn::RangeLimits) {
        // SPECIAL: NO SPAN
    }
    fn visit_receiver(&mut self, i: &'ast syn::Receiver) {
        visit![self, i, visit_receiver];
    }
    fn visit_return_type(&mut self, i: &'ast syn::ReturnType) {
        // SPECIAL: EMPTY SPAN
        if let syn::ReturnType::Default = i {
            return;
        }
        visit![self, i, visit_return_type];
    }
    fn visit_signature(&mut self, i: &'ast syn::Signature) {
        visit![self, i, visit_signature];
    }
    fn visit_span(&mut self, _i: &proc_macro2::Span) {
        // SPECIAL: OMITTED
    }
    fn visit_stmt(&mut self, i: &'ast syn::Stmt) {
        visit![self, i, visit_stmt];
    }
    fn visit_trait_bound(&mut self, i: &'ast syn::TraitBound) {
        visit![self, i, visit_trait_bound];
    }
    fn visit_trait_bound_modifier(&mut self, i: &'ast syn::TraitBoundModifier) {
        // SPECIAL: EMPTY SPAN
        if let syn::TraitBoundModifier::None = i {
            return;
        }
        visit![self, i, visit_trait_bound_modifier];
    }
    fn visit_trait_item(&mut self, i: &'ast syn::TraitItem) {
        visit![self, i, visit_trait_item];
    }
    fn visit_trait_item_const(&mut self, i: &'ast syn::TraitItemConst) {
        visit![self, i, visit_trait_item_const];
    }
    fn visit_trait_item_macro(&mut self, i: &'ast syn::TraitItemMacro) {
        visit![self, i, visit_trait_item_macro];
    }
    fn visit_trait_item_method(&mut self, i: &'ast syn::TraitItemMethod) {
        visit![self, i, visit_trait_item_method];
    }
    fn visit_trait_item_type(&mut self, i: &'ast syn::TraitItemType) {
        visit![self, i, visit_trait_item_type];
    }
    fn visit_type(&mut self, i: &'ast syn::Type) {
        visit![self, i, visit_type];
    }
    fn visit_type_array(&mut self, i: &'ast syn::TypeArray) {
        visit![self, i, visit_type_array];
    }
    fn visit_type_bare_fn(&mut self, i: &'ast syn::TypeBareFn) {
        visit![self, i, visit_type_bare_fn];
    }
    fn visit_type_group(&mut self, i: &'ast syn::TypeGroup) {
        visit![self, i, visit_type_group];
    }
    fn visit_type_impl_trait(&mut self, i: &'ast syn::TypeImplTrait) {
        visit![self, i, visit_type_impl_trait];
    }
    fn visit_type_infer(&mut self, i: &'ast syn::TypeInfer) {
        visit![self, i, visit_type_infer];
    }
    fn visit_type_macro(&mut self, i: &'ast syn::TypeMacro) {
        visit![self, i, visit_type_macro];
    }
    fn visit_type_never(&mut self, i: &'ast syn::TypeNever) {
        visit![self, i, visit_type_never];
    }
    fn visit_type_param(&mut self, i: &'ast syn::TypeParam) {
        visit![self, i, visit_type_param];
    }
    fn visit_type_param_bound(&mut self, i: &'ast syn::TypeParamBound) {
        visit![self, i, visit_type_param_bound];
    }
    fn visit_type_paren(&mut self, i: &'ast syn::TypeParen) {
        visit![self, i, visit_type_paren];
    }
    fn visit_type_path(&mut self, i: &'ast syn::TypePath) {
        visit![self, i, visit_type_path];
    }
    fn visit_type_ptr(&mut self, i: &'ast syn::TypePtr) {
        visit![self, i, visit_type_ptr];
    }
    fn visit_type_reference(&mut self, i: &'ast syn::TypeReference) {
        visit![self, i, visit_type_reference];
    }
    fn visit_type_slice(&mut self, i: &'ast syn::TypeSlice) {
        visit![self, i, visit_type_slice];
    }
    fn visit_type_trait_object(&mut self, i: &'ast syn::TypeTraitObject) {
        visit![self, i, visit_type_trait_object];
    }
    fn visit_type_tuple(&mut self, i: &'ast syn::TypeTuple) {
        visit![self, i, visit_type_tuple];
    }
    fn visit_un_op(&mut self, i: &'ast syn::UnOp) {
        visit![self, i, visit_un_op];
    }
    fn visit_use_glob(&mut self, i: &'ast syn::UseGlob) {
        visit![self, i, visit_use_glob];
    }
    fn visit_use_group(&mut self, i: &'ast syn::UseGroup) {
        visit![self, i, visit_use_group];
    }
    fn visit_use_name(&mut self, i: &'ast syn::UseName) {
        visit![self, i, visit_use_name];
    }
    fn visit_use_path(&mut self, i: &'ast syn::UsePath) {
        visit![self, i, visit_use_path];
    }
    fn visit_use_rename(&mut self, i: &'ast syn::UseRename) {
        visit![self, i, visit_use_rename];
    }
    fn visit_use_tree(&mut self, i: &'ast syn::UseTree) {
        visit![self, i, visit_use_tree];
    }
    fn visit_variadic(&mut self, i: &'ast syn::Variadic) {
        visit![self, i, visit_variadic];
    }
    fn visit_variant(&mut self, i: &'ast syn::Variant) {
        visit![self, i, visit_variant];
    }
    fn visit_vis_crate(&mut self, i: &'ast syn::VisCrate) {
        visit![self, i, visit_vis_crate];
    }
    fn visit_vis_public(&mut self, i: &'ast syn::VisPublic) {
        visit![self, i, visit_vis_public];
    }
    fn visit_vis_restricted(&mut self, i: &'ast syn::VisRestricted) {
        visit![self, i, visit_vis_restricted];
    }
    fn visit_visibility(&mut self, i: &'ast syn::Visibility) {
        // SPECIAL: EMPTY SPAN
        if let syn::Visibility::Inherited = i {
            return;
        }
        visit![self, i, visit_visibility];
    }
    fn visit_where_clause(&mut self, i: &'ast syn::WhereClause) {
        visit![self, i, visit_where_clause];
    }
    fn visit_where_predicate(&mut self, i: &'ast syn::WherePredicate) {
        visit![self, i, visit_where_predicate];
    }
}

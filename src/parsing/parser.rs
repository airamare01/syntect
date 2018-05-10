use super::syntax_definition::*;
use super::scope::*;
use onig::{MatchParam, Region, SearchOptions};
use std::usize;
use std::collections::HashMap;
use std::i32;
use std::hash::BuildHasherDefault;
use std::ptr;
use fnv::FnvHasher;

/// Keeps the current parser state (the internal syntax interpreter stack) between lines of parsing.
/// If you are parsing an entire file you create one of these at the start and use it
/// all the way to the end.
///
/// # Caching
///
/// One reason this is exposed is that since it implements `Clone` you can actually cache
/// these (probably along with a `HighlightState`) and only re-start parsing from the point of a change.
/// See the docs for `HighlightState` for more in-depth discussion of caching.
///
/// This state doesn't keep track of the current scope stack and parsing only returns changes to this stack
/// so if you want to construct scope stacks you'll need to keep track of that as well.
/// Note that `HighlightState` contains exactly this as a public field that you can use.
///
/// **Note:** Caching is for advanced users who have tons of time to maximize performance or want to do so eventually.
/// It is not recommended that you try caching the first time you implement highlighting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseState {
    stack: Vec<StateLevel>,
    first_line: bool,
    // See issue #101. Contains indices of frames pushed by `with_prototype`s.
    // Doesn't look at `with_prototype`s below top of stack.
    proto_starts: Vec<usize>,
}

#[derive(Debug, Clone)]
struct StateLevel {
    context: ContextPtr,
    prototype: Option<ContextPtr>,
    captures: Option<(Region, String)>,
}

fn context_ptr_eq(a: &ContextPtr, b: &ContextPtr) -> bool {
    ptr::eq(a.as_ptr(), b.as_ptr())
}

impl PartialEq for StateLevel {
    fn eq(&self, other: &StateLevel) -> bool {
        context_ptr_eq(&self.context, &other.context) &&
        match (&self.prototype, &other.prototype) {
            (&Some(ref a), &Some(ref b)) => context_ptr_eq(a, b),
            (&None, &None) => true,
            _ => false,
        } &&
        self.captures == other.captures
    }
}

impl Eq for StateLevel {}

#[derive(Debug)]
struct RegexMatch {
    regions: Region,
    context: ContextPtr,
    pat_index: usize,
    from_with_prototype: bool,
    would_loop: bool,
}

/// maps the pattern to the start index, which is -1 if not found.
type SearchCache = HashMap<*const MatchPattern, Option<Region>, BuildHasherDefault<FnvHasher>>;

// To understand the implementation of this, here's an introduction to how
// Sublime Text syntax definitions work.
//
// Let's say we have the following made-up syntax definition:
//
//     contexts:
//       main:
//         - match: A
//           scope: scope.a.first
//           push: context-a
//         - match: b
//           scope: scope.b
//         - match: \w+
//           scope: scope.other
//       context-a:
//         - match: a+
//           scope: scope.a.rest
//         - match: (?=.)
//           pop: true
//
// There are two contexts, `main` and `context-a`. Each context contains a list
// of match rules with instructions for how to proceed.
//
// Let's say we have the input string " Aaaabxxx". We start at position 0 in
// the string. We keep a stack of contexts, which at the beginning is just main.
//
// So we start by looking at the top of the context stack (main), and look at
// the rules in order. The rule that wins is the first one that matches
// "earliest" in the input string. In our example:
//
// 1. The first one matches "A". Note that matches are not anchored, so this
//    matches at position 1.
// 2. The second one matches "b", so position 5. The first rule is winning.
// 3. The third one matches "\w+", so also position 1. But because the first
//    rule comes first, it wins.
//
// So now we execute the winning rule. Whenever we matched some text, we assign
// the scope (if there is one) to the matched text and advance our position to
// after the matched text. The scope is "scope.a.first" and our new position is
// after the "A", so 2. The "push" means that we should change our stack by
// pushing `context-a` on top of it.
//
// In the next step, we repeat the above, but now with the rules in `context-a`.
// The result is that we match "a+" and assign "scope.a.rest" to "aaa", and our
// new position is now after the "aaa". Note that there was no instruction for
// changing the stack, so we stay in that context.
//
// In the next step, the first rule doesn't match anymore, so we go to the next
// rule where "(?=.)" matches. The instruction is to "pop", which means we
// pop the top of our context stack, which means we're now back in main.
//
// This time in main, we match "b", and in the next step we match the rest with
// "\w+", and we're done.
//
//
// ## Preventing loops
//
// These are the basics of how matching works. Now, you saw that you can write
// patterns that result in an empty match and don't change the position. These
// are called non-consuming matches. The problem with them is that they could
// result in infinite loops. Let's look at a syntax where that is the case:
//
//     contexts:
//       main:
//         - match: (?=.)
//           push: test
//       test:
//         - match: \w+
//           scope: word
//         - match: (?=.)
//           pop: true
//
// This is a bit silly, but it's a minimal example for explaining how matching
// works in that case.
//
// Let's say we have the input string " hello". In `main`, our rule matches and
// we go into `test` and stay at position 0. Now, the best match is the rule
// with "pop". But if we used that rule, we'd pop back to `main` and would still
// be at the same position we started at! So this would be an infinite loop,
// which we don't want.
//
// So what Sublime Text does in case a looping rule "won":
//
// * If there's another rule that matches at the same position and does not
//   result in a loop, use that instead.
// * Otherwise, go to the next position and go through all the rules in the
//   current context again. Note that it means that the "pop" could again be the
//   winning rule, but that's ok as it wouldn't result in a loop anymore.
//
// So in our input string, we'd skip one character and try to match the rules
// again. This time, the "\w+" wins because it comes first.

impl ParseState {
    /// Create a state from a syntax, keeps its own reference counted
    /// pointer to the main context of the syntax.
    pub fn new(syntax: &SyntaxDefinition) -> ParseState {
        let start_state = StateLevel {
            // __start is a special context we add in yaml_load.rs
            context: syntax.contexts["__start"].clone(),
            prototype: None,
            captures: None,
        };
        ParseState {
            stack: vec![start_state],
            first_line: true,
            proto_starts: Vec::new(),
        }
    }

    /// Parses a single line of the file. Because of the way regex engines work you unfortunately
    /// have to pass in a single line contiguous in memory. This can be bad for really long lines.
    /// Sublime Text avoids this by just not highlighting lines that are too long (thousands of characters).
    ///
    /// For efficiency reasons this returns only the changes to the current scope at each point in the line.
    /// You can use `ScopeStack#apply` on each operation in succession to get the stack for a given point.
    /// Look at the code in `highlighter.rs` for an example of doing this for highlighting purposes.
    ///
    /// The vector is in order both by index to apply at (the `usize`) and also by order to apply them at a
    /// given index (e.g popping old scopes before pushing new scopes).
    pub fn parse_line(&mut self, line: &str) -> Vec<(usize, ScopeStackOp)> {
        assert!(self.stack.len() > 0,
                "Somehow main context was popped from the stack");
        let mut match_start = 0;
        let mut res = Vec::new();

        if self.first_line {
            let cur_level = &self.stack[self.stack.len() - 1];
            let context = cur_level.context.borrow();
            if !context.meta_content_scope.is_empty() {
                res.push((0, ScopeStackOp::Push(context.meta_content_scope[0])));
            }
            self.first_line = false;
        }

        let mut regions = Region::with_capacity(8);
        let fnv = BuildHasherDefault::<FnvHasher>::default();
        let mut search_cache: SearchCache = HashMap::with_capacity_and_hasher(128, fnv);
        // Used for detecting loops with push/pop, see long comment above.
        let mut non_consuming_push_at = (0, 0);

        while self.parse_next_token(line,
                                    &mut match_start,
                                    &mut search_cache,
                                    &mut regions,
                                    &mut non_consuming_push_at,
                                    &mut res) {
        }

        res
    }

    fn parse_next_token(&mut self,
                        line: &str,
                        start: &mut usize,
                        search_cache: &mut SearchCache,
                        regions: &mut Region,
                        non_consuming_push_at: &mut (usize, usize),
                        ops: &mut Vec<(usize, ScopeStackOp)>)
                        -> bool {
        let check_pop_loop = {
            let (pos, stack_depth) = *non_consuming_push_at;
            pos == *start && stack_depth == self.stack.len()
        };

        // Trim proto_starts that are no longer valid
        while self.proto_starts.last().map(|start| *start >= self.stack.len()).unwrap_or(false) {
            self.proto_starts.pop();
        }

        let best_match = self.find_best_match(line, *start, search_cache, regions, check_pop_loop);

        if let Some(reg_match) = best_match {
            if reg_match.would_loop {
                // A push that doesn't consume anything (a regex that resulted
                // in an empty match at the current position) can not be
                // followed by a non-consuming pop. Otherwise we're back where
                // we started and would try the same sequence of matches again,
                // resulting in an infinite loop. In this case, Sublime Text
                // advances one character and tries again, thus preventing the
                // loop.

                // println!("pop_would_loop for match {:?}, start {}", reg_match, *start);

                if *start == line.len() {
                    // End of line, no character to advance and no point trying
                    // any more patterns.
                    return false;
                }
                *start += 1;
                return true;
            }

            let match_end = reg_match.regions.pos(0).unwrap().1;

            let consuming = match_end > *start;
            if !consuming {
                // The match doesn't consume any characters. If this is a
                // "push", remember the position and stack size so that we can
                // check the next "pop" for loops. Otherwise leave the state,
                // e.g. non-consuming "set" could also result in a loop.
                let context = reg_match.context.borrow();
                let match_pattern = context.match_at(reg_match.pat_index);
                if let MatchOperation::Push(_) = match_pattern.operation {
                    *non_consuming_push_at = (match_end, self.stack.len() + 1);
                }
            }

            *start = match_end;

            // ignore `with_prototype`s below this if a context is pushed
            if reg_match.from_with_prototype {
                // use current height, since we're before the actual push
                self.proto_starts.push(self.stack.len());
            }

            let level_context = self.stack[self.stack.len() - 1].context.clone();
            self.exec_pattern(line, reg_match, level_context, ops);

            true
        } else {
            false
        }
    }

    fn find_best_match(&self,
                       line: &str,
                       start: usize,
                       search_cache: &mut SearchCache,
                       regions: &mut Region,
                       check_pop_loop: bool)
                       -> Option<RegexMatch> {
        let cur_level = &self.stack[self.stack.len() - 1];
        let prototype: Option<ContextPtr> = {
            let ctx_ref = cur_level.context.borrow();
            ctx_ref.prototype.clone()
        };

        // Build an iterator for the contexts we want to visit in order
        let context_chain = {
            let proto_start = self.proto_starts.last().cloned().unwrap_or(0);
            // Sublime applies with_prototypes from bottom to top
            let with_prototypes = self.stack[proto_start..].iter().filter_map(|lvl| lvl.prototype.as_ref().map(|ctx| (true, ctx.clone(), lvl.captures.as_ref())));
            let cur_prototype = prototype.into_iter().map(|ctx| (false, ctx, None));
            let cur_context = Some((false, cur_level.context.clone(), cur_level.captures.as_ref())).into_iter();
            with_prototypes.chain(cur_prototype).chain(cur_context)
        };

        // println!("{:#?}", cur_level);
        // println!("token at {} on {}", start, line.trim_right());

        let mut min_start = usize::MAX;
        let mut best_match: Option<RegexMatch> = None;
        let mut pop_would_loop = false;

        for (from_with_proto, ctx, captures) in context_chain {
            for (pat_context_ptr, pat_index) in context_iter(ctx) {
                let mut pat_context = pat_context_ptr.borrow_mut();
                let match_pat = pat_context.match_at_mut(pat_index);

                if let Some(match_region) = self.search(
                    line, start, match_pat, captures, search_cache, regions
                ) {
                    let (match_start, match_end) = match_region.pos(0).unwrap();

                    // println!("matched pattern {:?} at start {} end {}", match_pat.regex_str, match_start, match_end);

                    if match_start < min_start || (match_start == min_start && pop_would_loop) {
                        // New match is earlier in text than old match,
                        // or old match was a looping pop at the same
                        // position.

                        // println!("setting as current match");

                        min_start = match_start;

                        let consuming = match_end > start;
                        pop_would_loop = check_pop_loop && !consuming && match match_pat.operation {
                            MatchOperation::Pop => true,
                            _ => false,
                        };

                        best_match = Some(RegexMatch {
                            regions: match_region,
                            context: pat_context_ptr.clone(),
                            pat_index,
                            from_with_prototype: from_with_proto,
                            would_loop: pop_would_loop,
                        });

                        if match_start == start && !pop_would_loop {
                            // We're not gonna find a better match after this,
                            // so as an optimization we can stop matching now.
                            return best_match;
                        }
                    }
                }
            }
        }
        best_match
    }

    fn search(&self,
              line: &str,
              start: usize,
              match_pat: &mut MatchPattern,
              captures: Option<&(Region, String)>,
              search_cache: &mut SearchCache,
              regions: &mut Region)
              -> Option<Region> {
        // println!("{} - {:?} - {:?}", match_pat.regex_str, match_pat.has_captures, cur_level.captures.is_some());
        let match_ptr = match_pat as *const MatchPattern;

        if let Some(maybe_region) = search_cache.get(&match_ptr) {
            if let Some(ref region) = *maybe_region {
                let match_start = region.pos(0).unwrap().0;
                if match_start >= start {
                    // Cached match is valid, return it. Otherwise do another
                    // search below.
                    return Some(region.clone());
                }
            } else {
                // Didn't find a match earlier, so no point trying to match it again
                return None;
            }
        }

        match_pat.ensure_compiled_if_possible();
        let refs_regex = if match_pat.has_captures && captures.is_some() {
            let &(ref region, ref s) = captures.unwrap();
            Some(match_pat.compile_with_refs(region, s))
        } else {
            None
        };
        let regex = if let Some(ref rgx) = refs_regex {
            rgx
        } else {
            match_pat.regex.as_ref().unwrap()
        };
        let matched = regex.search_with_param(line,
                                              start,
                                              line.len(),
                                              SearchOptions::SEARCH_OPTION_NONE,
                                              Some(regions),
                                              MatchParam::default());

        // If there's an error during search, treat it as non-matching.
        // For example, in case of catastrophic backtracking, onig should
        // fail with a "retry-limit-in-match over" error eventually.
        if let Ok(Some(match_start)) = matched {
            let match_end = regions.pos(0).unwrap().1;
            // this is necessary to avoid infinite looping on dumb patterns
            let does_something = match match_pat.operation {
                MatchOperation::None => match_start != match_end,
                _ => true,
            };
            if refs_regex.is_none() && does_something {
                search_cache.insert(match_pat, Some(regions.clone()));
            }
            if does_something {
                // print!("catch {} at {} on {}", match_pat.regex_str, match_start, line);
                return Some(regions.clone());
            }
        } else if refs_regex.is_none() {
            search_cache.insert(match_pat, None);
        }
        return None;
    }

    /// Returns true if the stack was changed
    fn exec_pattern(&mut self,
                    line: &str,
                    reg_match: RegexMatch,
                    level_context_ptr: ContextPtr,
                    ops: &mut Vec<(usize, ScopeStackOp)>)
                    -> bool {
        let (match_start, match_end) = reg_match.regions.pos(0).unwrap();
        let context = reg_match.context.borrow();
        let pat = context.match_at(reg_match.pat_index);
        let level_context = level_context_ptr.borrow();
        // println!("running pattern {:?} on '{}' at {}, operation {:?}", pat.regex_str, line, match_start, pat.operation);

        self.push_meta_ops(true, match_start, &*level_context, &pat.operation, ops);
        for s in &pat.scope {
            // println!("pushing {:?} at {}", s, match_start);
            ops.push((match_start, ScopeStackOp::Push(*s)));
        }
        if let Some(ref capture_map) = pat.captures {
            // captures could appear in an arbitrary order, have to produce ops in right order
            // ex: ((bob)|(hi))* could match hibob in wrong order, and outer has to push first
            // we don't have to handle a capture matching multiple times, Sublime doesn't
            let mut map: Vec<((usize, i32), ScopeStackOp)> = Vec::new();
            for &(cap_index, ref scopes) in capture_map.iter() {
                if let Some((cap_start, cap_end)) = reg_match.regions.pos(cap_index) {
                    // marking up empty captures causes pops to be sorted wrong
                    if cap_start == cap_end {
                        continue;
                    }
                    // println!("capture {:?} at {:?}-{:?}", scopes[0], cap_start, cap_end);
                    for scope in scopes.iter() {
                        map.push(((cap_start, -((cap_end - cap_start) as i32)),
                                  ScopeStackOp::Push(*scope)));
                    }
                    map.push(((cap_end, i32::MIN), ScopeStackOp::Pop(scopes.len())));
                }
            }
            map.sort_by(|a, b| a.0.cmp(&b.0));
            for ((index, _), op) in map.into_iter() {
                ops.push((index, op));
            }
        }
        if !pat.scope.is_empty() {
            // println!("popping at {}", match_end);
            ops.push((match_end, ScopeStackOp::Pop(pat.scope.len())));
        }
        self.push_meta_ops(false, match_end, &*level_context, &pat.operation, ops);

        self.perform_op(line, &reg_match.regions, pat)
    }

    fn push_meta_ops(&self,
                     initial: bool,
                     index: usize,
                     cur_context: &Context,
                     match_op: &MatchOperation,
                     ops: &mut Vec<(usize, ScopeStackOp)>) {
        // println!("metas ops for {:?}, initial: {}",
        //          match_op,
        //          initial);
        // println!("{:?}", cur_context.meta_scope);
        match *match_op {
            MatchOperation::Pop => {
                let v = if initial {
                    &cur_context.meta_content_scope
                } else {
                    &cur_context.meta_scope
                };
                if !v.is_empty() {
                    ops.push((index, ScopeStackOp::Pop(v.len())));
                }

                // cleared scopes are restored after the scopes from match pattern that invoked the pop are applied
                if !initial && cur_context.clear_scopes != None {
                    ops.push((index, ScopeStackOp::Restore));
                }
            },
            // for some reason the ST3 behaviour of set is convoluted and is inconsistent with the docs and other ops
            // - the meta_content_scope of the current context is applied to the matched thing, unlike pop
            // - the clear_scopes are applied after the matched token, unlike push
            // - the interaction with meta scopes means that the token has the meta scopes of both the current scope and the new scope.
            MatchOperation::Push(ref context_refs) |
            MatchOperation::Set(ref context_refs) => {
                let is_set = match *match_op {
                    MatchOperation::Set(_) => true,
                    _ => false
                };
                // a match pattern that "set"s keeps the meta_content_scope and meta_scope from the previous context
                if initial {
                    // add each context's meta scope
                    for r in context_refs.iter() {
                        let ctx_ptr = r.resolve();
                        let ctx = ctx_ptr.borrow();

                        if !is_set {
                            if let Some(clear_amount) = ctx.clear_scopes {
                                ops.push((index, ScopeStackOp::Clear(clear_amount)));
                            }
                        }

                        for scope in ctx.meta_scope.iter() {
                            ops.push((index, ScopeStackOp::Push(*scope)));
                        }
                    }
                } else {
                    let repush = (is_set && (!cur_context.meta_scope.is_empty() || !cur_context.meta_content_scope.is_empty())) || context_refs.iter().any(|r| {
                        let ctx_ptr = r.resolve();
                        let ctx = ctx_ptr.borrow();

                        !ctx.meta_content_scope.is_empty() || (ctx.clear_scopes.is_some() && is_set)
                    });
                    if repush {
                        // remove previously pushed meta scopes, so that meta content scopes will be applied in the correct order
                        let mut num_to_pop : usize = context_refs.iter().map(|r| {
                            let ctx_ptr = r.resolve();
                            let ctx = ctx_ptr.borrow();
                            ctx.meta_scope.len()
                        }).sum();

                        // also pop off the original context's meta scopes
                        if is_set {
                            num_to_pop += cur_context.meta_content_scope.len() + cur_context.meta_scope.len();
                        }

                        // do all the popping as one operation
                        if num_to_pop > 0 {
                            ops.push((index, ScopeStackOp::Pop(num_to_pop)));
                        }

                        // now we push meta scope and meta context scope for each context pushed
                        for r in context_refs {
                            let ctx_ptr = r.resolve();
                            let ctx = ctx_ptr.borrow();

                            // for some reason, contrary to my reading of the docs, set does this after the token
                            if is_set {
                                if let Some(clear_amount) = ctx.clear_scopes {
                                    ops.push((index, ScopeStackOp::Clear(clear_amount)));
                                }
                            }

                            for scope in ctx.meta_scope.iter() {
                                ops.push((index, ScopeStackOp::Push(*scope)));
                            }
                            for scope in ctx.meta_content_scope.iter() {
                                ops.push((index, ScopeStackOp::Push(*scope)));
                            }
                        }
                    }
                }
            },
            MatchOperation::None => (),
        }
    }

    /// Returns true if the stack was changed
    fn perform_op(&mut self, line: &str, regions: &Region, pat: &MatchPattern) -> bool {
        let ctx_refs = match pat.operation {
            MatchOperation::Push(ref ctx_refs) => ctx_refs,
            MatchOperation::Set(ref ctx_refs) => {
                self.stack.pop();
                ctx_refs
            }
            MatchOperation::Pop => {
                self.stack.pop();
                return true;
            }
            MatchOperation::None => return false,
        };
        for (i, r) in ctx_refs.iter().enumerate() {
            // if a with_prototype was specified, and multiple contexts were pushed,
            // then the with_prototype applies only to the last context pushed, i.e.
            // top most on the stack after all the contexts are pushed - this is also
            // referred to as the "target" of the push by sublimehq - see
            // https://forum.sublimetext.com/t/dev-build-3111/19240/17 for more info
            let proto = if i == ctx_refs.len() - 1 {
                pat.with_prototype.clone()
            } else {
                None
            };
            let ctx_ptr = r.resolve();
            let captures = {
                let mut uses_backrefs = ctx_ptr.borrow().uses_backrefs;
                if let Some(ref proto) = proto {
                    uses_backrefs = uses_backrefs || proto.borrow().uses_backrefs;
                }
                if uses_backrefs {
                    Some((regions.clone(), line.to_owned()))
                } else {
                    None
                }
            };
            self.stack.push(StateLevel {
                context: ctx_ptr,
                prototype: proto,
                captures,
            });
        }
        true
    }
}

#[cfg(feature = "yaml-load")]
#[cfg(test)]
mod tests {
    use super::*;
    use parsing::{SyntaxSet, Scope, ScopeStack};
    use parsing::ScopeStackOp::{Push, Pop, Clear, Restore};
    use util::debug_print_ops;

    const TEST_SYNTAX: &str = include_str!("../../testdata/parser_tests.sublime-syntax");

    #[test]
    fn can_parse_simple() {
        let ps = SyntaxSet::load_from_folder("testdata/Packages").unwrap();
        let mut state = {
            let syntax = ps.find_syntax_by_name("Ruby on Rails").unwrap();
            ParseState::new(syntax)
        };

        let ops1 = ops("module Bob::Wow::Troll::Five; 5; end", &mut state);
        let test_ops1 = vec![
            (0, Push(Scope::new("source.ruby.rails").unwrap())),
            (0, Push(Scope::new("meta.module.ruby").unwrap())),
            (0, Push(Scope::new("keyword.control.module.ruby").unwrap())),
            (6, Pop(2)),
            (6, Push(Scope::new("meta.module.ruby").unwrap())),
            (7, Pop(1)),
            (7, Push(Scope::new("meta.module.ruby").unwrap())),
            (7, Push(Scope::new("entity.name.module.ruby").unwrap())),
            (7, Push(Scope::new("support.other.namespace.ruby").unwrap())),
            (10, Pop(1)),
            (10, Push(Scope::new("punctuation.accessor.ruby").unwrap())),
        ];
        assert_eq!(&ops1[0..test_ops1.len()], &test_ops1[..]);

        let ops2 = ops("def lol(wow = 5)", &mut state);
        let test_ops2 = vec![
            (0, Push(Scope::new("meta.function.ruby").unwrap())),
            (0, Push(Scope::new("keyword.control.def.ruby").unwrap())),
            (3, Pop(2)),
            (3, Push(Scope::new("meta.function.ruby").unwrap())),
            (4, Push(Scope::new("entity.name.function.ruby").unwrap())),
            (7, Pop(1))
        ];
        assert_eq!(&ops2[0..test_ops2.len()], &test_ops2[..]);
    }

    #[test]
    fn can_parse_includes() {
        let ps = SyntaxSet::load_from_folder("testdata/Packages").unwrap();
        let mut state = {
            let syntax = ps.find_syntax_by_name("HTML (Rails)").unwrap();
            ParseState::new(syntax)
        };

        let ops = ops("<script>var lol = '<% def wow(", &mut state);

        let mut test_stack = ScopeStack::new();
        test_stack.push(Scope::new("text.html.ruby").unwrap());
        test_stack.push(Scope::new("text.html.basic").unwrap());
        test_stack.push(Scope::new("source.js.embedded.html").unwrap());
        test_stack.push(Scope::new("source.js").unwrap());
        test_stack.push(Scope::new("string.quoted.single.js").unwrap());
        test_stack.push(Scope::new("source.ruby.rails.embedded.html").unwrap());
        test_stack.push(Scope::new("meta.function.parameters.ruby").unwrap());

        let mut stack = ScopeStack::new();
        for &(_, ref op) in ops.iter() {
            stack.apply(op);
        }
        assert_eq!(stack, test_stack);
    }

    #[test]
    fn can_parse_backrefs() {
        let ps = SyntaxSet::load_from_folder("testdata/Packages").unwrap();
        let mut state = {
            let syntax = ps.find_syntax_by_name("Ruby on Rails").unwrap();
            ParseState::new(syntax)
        };

        // For parsing HEREDOC, the "SQL" is captured at the beginning and then used in another
        // regex with a backref, to match the end of the HEREDOC. Note that there can be code
        // after the marker (`.strip`) here.
        assert_eq!(ops("lol = <<-SQL.strip", &mut state), vec![
            (0, Push(Scope::new("source.ruby.rails").unwrap())),
            (4, Push(Scope::new("keyword.operator.assignment.ruby").unwrap())),
            (5, Pop(1)),
            (6, Push(Scope::new("string.unquoted.embedded.sql.ruby").unwrap())),
            (6, Push(Scope::new("punctuation.definition.string.begin.ruby").unwrap())),
            (12, Pop(1)),
            (12, Pop(1)),
            (12, Push(Scope::new("string.unquoted.embedded.sql.ruby").unwrap())),
            (12, Push(Scope::new("text.sql.embedded.ruby").unwrap())),
            (12, Clear(ClearAmount::TopN(2))),
            (12, Push(Scope::new("punctuation.accessor.ruby").unwrap())),
            (13, Pop(1)),
            (18, Restore),
        ]);

        assert_eq!(ops("wow", &mut state), vec![]);

        assert_eq!(ops("SQL", &mut state), vec![
            (0, Pop(1)),
            (0, Push(Scope::new("punctuation.definition.string.end.ruby").unwrap())),
            (3, Pop(1)),
            (3, Pop(1)),
        ]);
    }

    #[test]
    fn can_parse_preprocessor_rules() {
        let ps = SyntaxSet::load_from_folder("testdata/Packages").unwrap();
        let mut state = {
            let syntax = ps.find_syntax_by_name("C").unwrap();
            ParseState::new(syntax)
        };

        assert_eq!(ops("#ifdef FOO", &mut state), vec![
            (0, Push(Scope::new("source.c").unwrap())),
            (0, Push(Scope::new("meta.preprocessor.c").unwrap())),
            (0, Push(Scope::new("keyword.control.import.c").unwrap())),
            (6, Pop(1)),
            (10, Pop(1)),
        ]);
        assert_eq!(ops("{", &mut state), vec![
            (0, Push(Scope::new("meta.block.c").unwrap())),
            (0, Push(Scope::new("punctuation.section.block.begin.c").unwrap())),
            (1, Pop(1)),
        ]);
        assert_eq!(ops("#else", &mut state), vec![
            (0, Push(Scope::new("meta.preprocessor.c").unwrap())),
            (0, Push(Scope::new("keyword.control.import.c").unwrap())),
            (5, Pop(1)),
            (5, Pop(1)),
        ]);
        assert_eq!(ops("{", &mut state), vec![
            (0, Push(Scope::new("meta.block.c").unwrap())),
            (0, Push(Scope::new("punctuation.section.block.begin.c").unwrap())),
            (1, Pop(1)),
        ]);
        assert_eq!(ops("#endif", &mut state), vec![
            (0, Pop(1)),
            (0, Push(Scope::new("meta.block.c").unwrap())),
            (0, Push(Scope::new("meta.preprocessor.c").unwrap())),
            (0, Push(Scope::new("keyword.control.import.c").unwrap())),
            (6, Pop(2)),
            (6, Pop(2)),
            (6, Push(Scope::new("meta.block.c").unwrap())),
        ]);
        assert_eq!(ops("    foo;", &mut state), vec![
            (7, Push(Scope::new("punctuation.terminator.c").unwrap())),
            (8, Pop(1)),
        ]);
        assert_eq!(ops("}", &mut state), vec![
            (0, Push(Scope::new("punctuation.section.block.end.c").unwrap())),
            (1, Pop(1)),
            (1, Pop(1)),
        ]);
    }

    #[test]
    fn can_parse_issue25() {
        let ps = SyntaxSet::load_from_folder("testdata/Packages").unwrap();
        let mut state = {
            let syntax = ps.find_syntax_by_name("C").unwrap();
            ParseState::new(syntax)
        };

        // test fix for issue #25
        assert_eq!(ops("struct{estruct", &mut state).len(), 10);
    }

    #[test]
    fn can_parse_non_nested_clear_scopes() {
        let line = "'hello #simple_cleared_scopes_test world test \\n '";
        let expect = [
            "<source.test>, <example.meta-scope.after-clear-scopes.example>, <example.pushes-clear-scopes.example>",
            "<source.test>, <example.meta-scope.after-clear-scopes.example>, <example.pops-clear-scopes.example>",
            "<source.test>, <string.quoted.single.example>, <constant.character.escape.example>",
        ];
        expect_scope_stacks(&line, &expect, TEST_SYNTAX);
    }

    #[test]
    fn can_parse_non_nested_too_many_clear_scopes() {
        let line = "'hello #too_many_cleared_scopes_test world test \\n '";
        let expect = [
            "<example.meta-scope.after-clear-scopes.example>, <example.pushes-clear-scopes.example>",
            "<example.meta-scope.after-clear-scopes.example>, <example.pops-clear-scopes.example>",
            "<source.test>, <string.quoted.single.example>, <constant.character.escape.example>",
        ];
        expect_scope_stacks(&line, &expect, TEST_SYNTAX);
    }

    #[test]
    fn can_parse_nested_clear_scopes() {
        let line = "'hello #nested_clear_scopes_test world foo bar test \\n '";
        let expect = [
            "<source.test>, <example.meta-scope.after-clear-scopes.example>, <example.pushes-clear-scopes.example>",
            "<source.test>, <example.meta-scope.cleared-previous-meta-scope.example>, <foo>",
            "<source.test>, <example.meta-scope.after-clear-scopes.example>, <example.pops-clear-scopes.example>",
            "<source.test>, <string.quoted.single.example>, <constant.character.escape.example>",
        ];
        expect_scope_stacks(&line, &expect, TEST_SYNTAX);
    }

    #[test]
    fn can_parse_infinite_loop() {
        let line = "#infinite_loop_test 123";
        let expect = [
            "<source.test>, <constant.numeric.test>",
        ];
        expect_scope_stacks(&line, &expect, TEST_SYNTAX);
    }

    #[test]
    fn can_parse_infinite_seeming_loop() {
        // See https://github.com/SublimeTextIssues/Core/issues/1190 for an
        // explanation.
        let line = "#infinite_seeming_loop_test hello";
        let expect = [
            "<source.test>, <keyword.test>",
            "<source.test>, <test>, <string.unquoted.test>",
            "<source.test>, <test>, <keyword.control.test>",
        ];
        expect_scope_stacks(&line, &expect, TEST_SYNTAX);
    }

    #[test]
    fn can_parse_prototype_that_pops_main() {
        let syntax = r#"
name: test
scope: source.test
contexts:
  prototype:
    # This causes us to pop out of the main context. Sublime Text handles that
    # by pushing main back automatically.
    - match: (?=!)
      pop: true
  main:
    - match: foo
      scope: test.good
"#;

        let line = "foo!";
        let expect = ["<source.test>, <test.good>"];
        expect_scope_stacks(&line, &expect, syntax);
    }

    #[test]
    fn can_parse_syntax_with_newline_in_character_class() {
        let syntax = r#"
name: test
scope: source.test
contexts:
  main:
    - match: foo[\n]
      scope: foo.end
    - match: foo
      scope: foo.any
"#;

        let line = "foo";
        let expect = ["<source.test>, <foo.end>"];
        expect_scope_stacks(&line, &expect, syntax);

        let line = "foofoofoo";
        let expect = [
            "<source.test>, <foo.any>",
            "<source.test>, <foo.any>",
            "<source.test>, <foo.end>",
        ];
        expect_scope_stacks(&line, &expect, syntax);
    }

    #[test]
    fn can_parse_issue120() {
        let ps = SyntaxSet::load_from_folder("testdata").unwrap();
        let syntax = ps.find_syntax_by_name("Embed_Escape Used by tests in src/parsing/parser.rs").unwrap();

        let line1 = "\"abctest\" foobar";
        let expect1 = [
            "<meta.attribute-with-value.style.html>, <string.quoted.double>, <punctuation.definition.string.begin.html>",
            "<meta.attribute-with-value.style.html>, <source.css>",
            "<meta.attribute-with-value.style.html>, <string.quoted.double>, <punctuation.definition.string.end.html>",
            "<meta.attribute-with-value.style.html>, <source.css>, <test.embedded>",
            "<top-level.test>",
        ];
        expect_scope_stacks_with_syntax(&line1, &expect1, syntax.to_owned());

        let line2 = ">abctest</style>foobar";
        let expect2 = [
            "<meta.tag.style.begin.html>, <punctuation.definition.tag.end.html>",
            "<source.css.embedded.html>, <test.embedded>",
            "<top-level.test>",
        ];
        expect_scope_stacks_with_syntax(&line2, &expect2, syntax.to_owned());
    }

    #[test]
    fn can_parse_non_consuming_pop_that_would_loop() {
        // See https://github.com/trishume/syntect/issues/127
        let syntax = r#"
name: test
scope: source.test
contexts:
  main:
    # This makes us go into "test" without consuming any characters
    - match: (?=hello)
      push: test
  test:
    # If we used this match, we'd go back to "main" without consuming anything,
    # and then back into "test", infinitely looping. ST detects this at this
    # point and ignores this match until at least one character matched.
    - match: (?!world)
      pop: true
    - match: \w+
      scope: test.matched
"#;

        let line = "hello";
        let expect = ["<source.test>, <test.matched>"];
        expect_scope_stacks(&line, &expect, syntax);
    }

    #[test]
    fn can_parse_non_consuming_set_and_pop_that_would_loop() {
        let syntax = r#"
name: test
scope: source.test
contexts:
  main:
    # This makes us go into "a" without advancing
    - match: (?=test)
      push: a
  a:
    # This makes us go into "b" without advancing
    - match: (?=t)
      set: b
  b:
    # If we used this match, we'd go back to "main" without having advanced,
    # which means we'd have an infinite loop like with the previous test.
    # So even for a "set", we have to check if we're advancing or not.
    - match: (?=t)
      pop: true
    - match: \w+
      scope: test.matched
"#;

        let line = "test";
        let expect = ["<source.test>, <test.matched>"];
        expect_scope_stacks(&line, &expect, syntax);
    }

    #[test]
    fn can_parse_non_consuming_set_after_consuming_push_that_does_not_loop() {
        let syntax = r#"
name: test
scope: source.test
contexts:
  main:
    # This makes us go into "a", but we consumed a character
    - match: t
      push: a
    - match: \w+
      scope: test.matched
  a:
    # This makes us go into "b" without consuming
    - match: (?=e)
      set: b
  b:
    # This match does not result in an infinite loop because we already consumed
    # a character to get into "a", so it's ok to pop back into "main".
    - match: (?=e)
      pop: true
"#;

        let line = "test";
        let expect = ["<source.test>, <test.matched>"];
        expect_scope_stacks(&line, &expect, syntax);
    }

    #[test]
    fn can_parse_non_consuming_set_after_consuming_set_that_does_not_loop() {
        let syntax = r#"
name: test
scope: source.test
contexts:
  main:
    - match: (?=hello)
      push: a
    - match: \w+
      scope: test.matched
  a:
    - match: h
      set: b
  b:
    - match: (?=e)
      set: c
  c:
    # This is not an infinite loop because "a" consumed a character, so we can
    # actually pop back into main and then match the rest of the input.
    - match: (?=e)
      pop: true
"#;

        let line = "hello";
        let expect = ["<source.test>, <test.matched>"];
        expect_scope_stacks(&line, &expect, syntax);
    }

    #[test]
    fn can_parse_non_consuming_pop_that_would_loop_at_end_of_line() {
        let syntax = r#"
name: test
scope: source.test
contexts:
  main:
    # This makes us go into "test" without consuming, even at the end of line
    - match: ""
      push: test
  test:
    - match: ""
      pop: true
    - match: \w+
      scope: test.matched
"#;

        let line = "hello";
        let expect = ["<source.test>, <test.matched>"];
        expect_scope_stacks(&line, &expect, syntax);
    }

    #[test]
    fn can_parse_empty_but_consuming_set_that_does_not_loop() {
        let syntax = r#"
name: test
scope: source.test
contexts:
  main:
    - match: (?=hello)
      push: a
    - match: ello
      scope: test.good
  a:
    # This is an empty match, but it consumed a character (the "h")
    - match: (?=e)
      set: b
  b:
    # .. so it's ok to pop back to main from here
    - match: ""
      pop: true
    - match: ello
      scope: test.bad
"#;

        let line = "hello";
        let expect = ["<source.test>, <test.good>"];
        expect_scope_stacks(&line, &expect, syntax);
    }

    #[test]
    fn can_parse_non_consuming_pop_that_does_not_loop() {
        let syntax = r#"
name: test
scope: source.test
contexts:
  main:
    # This is a non-consuming push, so "b" will need to check for a
    # non-consuming pop
    - match: (?=hello)
      push: [b, a]
    - match: ello
      scope: test.good
  a:
    # This pop is ok, it consumed "h"
    - match: (?=e)
      pop: true
  b:
    # This is non-consuming, and we set to "c"
    - match: (?=e)
      set: c
  c:
    # It's ok to pop back to "main" here because we consumed a character in the
    # meantime.
    - match: ""
      pop: true
    - match: ello
      scope: test.bad
"#;

        let line = "hello";
        let expect = ["<source.test>, <test.good>"];
        expect_scope_stacks(&line, &expect, syntax);
    }

    #[test]
    fn can_parse_non_consuming_pop_with_multi_push_that_does_not_loop() {
        let syntax = r#"
name: test
scope: source.test
contexts:
  main:
    - match: (?=hello)
      push: [b, a]
    - match: ello
      scope: test.good
  a:
    # This pop is ok, as we're not popping back to "main" yet (which would loop),
    # we're popping to "b"
    - match: ""
      pop: true
    - match: \w+
      scope: test.bad
  b:
    - match: \w+
      scope: test.good
"#;

        let line = "hello";
        let expect = ["<source.test>, <test.good>"];
        expect_scope_stacks(&line, &expect, syntax);
    }

    #[test]
    fn can_parse_non_consuming_pop_of_recursive_context_that_does_not_loop() {
        let syntax = r#"
name: test
scope: source.test
contexts:
  main:
    - match: xxx
      scope: test.good
    - include: basic-identifiers

  basic-identifiers:
    - match: '\w+::'
      scope: test.matched
      push: no-type-names

  no-type-names:
      - include: basic-identifiers
      - match: \w+
        scope: test.matched.inside
      # This is a tricky one because when this is the best match,
      # we have two instances of "no-type-names" on the stack, so we're popping
      # back from "no-type-names" to another "no-type-names".
      - match: ''
        pop: true
"#;

        let line = "foo::bar::* xxx";
        let expect = ["<source.test>, <test.good>"];
        expect_scope_stacks(&line, &expect, syntax);
    }

    #[test]
    fn can_parse_non_consuming_pop_order() {
        let syntax = r#"
name: test
scope: source.test
contexts:
  main:
    - match: (?=hello)
      push: test
  test:
    # This matches first
    - match: (?=e)
      push: good
    # But this (looping) match replaces it, because it's an earlier match
    - match: (?=h)
      pop: true
    # And this should not replace it, as it's a later match (only matches at
    # the same position can replace looping pops).
    - match: (?=o)
      push: bad
  good:
    - match: \w+
      scope: test.good
  bad:
    - match: \w+
      scope: test.bad
"#;

        let line = "hello";
        let expect = ["<source.test>, <test.good>"];
        expect_scope_stacks(&line, &expect, syntax);
    }

    fn expect_scope_stacks(line_without_newline: &str, expect: &[&str], syntax: &str) {
        println!("Parsing with newlines");
        let line_with_newline = format!("{}\n", line_without_newline);
        let syntax_newlines = SyntaxDefinition::load_from_str(&syntax, true, None).unwrap();
        expect_scope_stacks_with_syntax(&line_with_newline, expect, syntax_newlines);

        println!("Parsing without newlines");
        let syntax_nonewlines = SyntaxDefinition::load_from_str(&syntax, false, None).unwrap();
        expect_scope_stacks_with_syntax(&line_without_newline, expect, syntax_nonewlines);
    }

    fn expect_scope_stacks_with_syntax(line: &str, expect: &[&str], syntax: SyntaxDefinition) {
        // check that each expected scope stack appears at least once while parsing the given test line

        let mut syntax_set = SyntaxSet::new();
        syntax_set.add_syntax(syntax);
        syntax_set.link_syntaxes();

        let mut state = ParseState::new(&syntax_set.syntaxes()[0]);

        let mut stack = ScopeStack::new();
        let ops = ops(line, &mut state);

        let mut criteria_met = Vec::new();
        for &(_, ref op) in ops.iter() {
            stack.apply(op);
            let stack_str = format!("{:?}", stack);
            println!("{}", stack_str);
            for expectation in expect.iter() {
                if stack_str.contains(expectation) {
                    criteria_met.push(expectation);
                }
            }
        }
        if let Some(missing) = expect.iter().filter(|e| !criteria_met.contains(&e)).next() {
            panic!("expected scope stack '{}' missing", missing);
        }
    }

    fn ops(line: &str, state: &mut ParseState) -> Vec<(usize, ScopeStackOp)> {
        let ops = state.parse_line(line);
        debug_print_ops(line, &ops);
        ops
    }
}

//! Liquid engine setup: parser configuration + custom filters.
//!
//! We register a small library of case-conversion filters on top of the
//! standard liquid stdlib. These are the things every template wants and that
//! liquid can't express natively without ugly chained replaces.

use crate::error::{Error, Result};
use liquid::{Parser, ParserBuilder};
use liquid_core::{
    Display_filter, Filter, FilterReflection, ParseFilter, Result as LiquidResult, Runtime, Value,
    ValueView,
};
use std::path::Path;

/// Build a liquid parser with viscous's filter library registered.
pub fn parser() -> Result<Parser> {
    let builder = ParserBuilder::with_stdlib()
        .filter(SnakeCaseFilter)
        .filter(KebabCaseFilter)
        .filter(PascalCaseFilter)
        .filter(CamelCaseFilter)
        .filter(ShoutySnakeCaseFilter)
        .filter(TitleCaseFilter);
    builder.build().map_err(|e| Error::LiquidParse {
        path: std::path::PathBuf::from("<parser-setup>"),
        source: e,
    })
}

/// Render a single template source into a string.
pub fn render(parser: &Parser, source: &str, vars: &liquid::Object, path: &Path) -> Result<String> {
    let template = parser.parse(source).map_err(|source| Error::LiquidParse {
        path: path.to_path_buf(),
        source,
    })?;
    template.render(vars).map_err(|source| Error::LiquidRender {
        path: path.to_path_buf(),
        source,
    })
}

/// Render a value (not a file) against the given vars; used for dest-paths,
/// when-expressions, derived-vars, etc.
pub fn render_expr(parser: &Parser, expr: &str, vars: &liquid::Object) -> Result<String> {
    let template = parser.parse(expr).map_err(|source| Error::LiquidParse {
        path: std::path::PathBuf::from("<expr>"),
        source,
    })?;
    template.render(vars).map_err(|source| Error::LiquidRender {
        path: std::path::PathBuf::from("<expr>"),
        source,
    })
}

// ─── Filters ─────────────────────────────────────────────────────────────────
//
// Each filter is a pair: the parser type (ParseFilter + FilterReflection
// derive) and the filter impl. Written out explicitly rather than wrapped in
// a single macro, because rustfmt re-indents `#[filter(…)]` inside
// `macro_rules!` invocations on every run.

use heck::{
    ToKebabCase, ToLowerCamelCase, ToPascalCase, ToShoutySnakeCase, ToSnakeCase, ToTitleCase,
};

macro_rules! case_filter_impl {
    ($impl:ident, $name:literal, $func:expr) => {
        #[derive(Debug, Default, Display_filter)]
        #[name = $name]
        struct $impl;

        impl Filter for $impl {
            fn evaluate(
                &self,
                input: &dyn ValueView,
                _runtime: &dyn Runtime,
            ) -> LiquidResult<Value> {
                let s = input.to_kstr().into_owned();
                let func: fn(&str) -> String = $func;
                Ok(Value::scalar(func(&s)))
            }
        }
    };
}

#[derive(Clone, ParseFilter, FilterReflection)]
#[filter(
    name = "snake_case",
    description = "Convert string to snake_case",
    parsed(SnakeCaseImpl)
)]
pub struct SnakeCaseFilter;
case_filter_impl!(SnakeCaseImpl, "snake_case", |s| s.to_snake_case());

#[derive(Clone, ParseFilter, FilterReflection)]
#[filter(
    name = "kebab_case",
    description = "Convert string to kebab-case",
    parsed(KebabCaseImpl)
)]
pub struct KebabCaseFilter;
case_filter_impl!(KebabCaseImpl, "kebab_case", |s| s.to_kebab_case());

#[derive(Clone, ParseFilter, FilterReflection)]
#[filter(
    name = "pascal_case",
    description = "Convert string to PascalCase",
    parsed(PascalCaseImpl)
)]
pub struct PascalCaseFilter;
case_filter_impl!(PascalCaseImpl, "pascal_case", |s| s.to_pascal_case());

#[derive(Clone, ParseFilter, FilterReflection)]
#[filter(
    name = "camel_case",
    description = "Convert string to camelCase",
    parsed(CamelCaseImpl)
)]
pub struct CamelCaseFilter;
case_filter_impl!(CamelCaseImpl, "camel_case", |s| s.to_lower_camel_case());

#[derive(Clone, ParseFilter, FilterReflection)]
#[filter(
    name = "shouty_snake_case",
    description = "Convert string to SHOUTY_SNAKE_CASE",
    parsed(ShoutySnakeCaseImpl)
)]
pub struct ShoutySnakeCaseFilter;
case_filter_impl!(ShoutySnakeCaseImpl, "shouty_snake_case", |s| s
    .to_shouty_snake_case());

#[derive(Clone, ParseFilter, FilterReflection)]
#[filter(
    name = "title_case",
    description = "Convert string to Title Case",
    parsed(TitleCaseImpl)
)]
pub struct TitleCaseFilter;
case_filter_impl!(TitleCaseImpl, "title_case", |s| s.to_title_case());

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn case_filters_work() {
        let p = parser().unwrap();
        let mut obj = liquid::Object::new();
        obj.insert(
            "name".into(),
            liquid_core::Value::scalar("HelloWorld".to_string()),
        );

        for (expr, expected) in [
            ("{{ name | snake_case }}", "hello_world"),
            ("{{ name | kebab_case }}", "hello-world"),
            ("{{ name | pascal_case }}", "HelloWorld"),
            ("{{ name | camel_case }}", "helloWorld"),
            ("{{ name | shouty_snake_case }}", "HELLO_WORLD"),
            ("{{ name | title_case }}", "Hello World"),
        ] {
            let got = render(&p, expr, &obj, Path::new("test")).unwrap();
            assert_eq!(got, expected, "for {expr}");
        }
    }

    #[test]
    fn nested_objects_render() {
        let p = parser().unwrap();
        let mut obj = liquid::Object::new();
        let mut inner = liquid::Object::new();
        inner.insert(
            "name".into(),
            liquid_core::Value::scalar("Button".to_string()),
        );
        obj.insert("c".into(), liquid_core::Value::Object(inner));

        let got = render(&p, "{{ c.name | snake_case }}.rs", &obj, Path::new("test")).unwrap();
        assert_eq!(got, "button.rs");
    }
}

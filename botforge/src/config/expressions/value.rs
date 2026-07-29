use serde_yaml::Value as YamlValue;

/// A fully typed value returned by the expression evaluator.
///
/// Type is carried through evaluation and only flattened to a string at two moments:
/// - **interpolation into surrounding text** (`to_interpolated_string`)
/// - the explicit **`to_json`** function
///
/// Everything else (truthiness, equality, `&&`/`||`) works on the typed value directly.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum EvaluatedValue {
    String(std::string::String),
    Number(f64),
    Bool(bool),
    /// Represents an undefined/unset reference (soft-empty). Falsy.
    Empty,
}

impl EvaluatedValue {
    /// Truthiness: falsy = `""`, number `0`, `false`, `Empty`. Everything else truthy.
    ///
    /// Non-empty strings (including `"0"` and `"false"`) are truthy; only the
    /// NUMBER zero and BOOL false are falsy.
    pub(super) fn truthy(&self) -> bool {
        match self {
            Self::Empty => false,
            Self::Bool(flag) => *flag,
            Self::Number(number) => *number != 0.0,
            Self::String(text) => !text.is_empty(),
        }
    }

    /// Faithful string rendering for **interpolation into surrounding text**.
    ///
    /// Renders all typed values faithfully; ONLY `Empty` yields `""`.
    /// `Bool(false)` → `"false"`, `Number(0)` → `"0"`, `String(s)` → `s`.
    ///
    /// Must NOT be used for truthiness checks — use `truthy()` for that.
    pub(super) fn to_interpolated_string(&self) -> std::string::String {
        match self {
            Self::Empty => std::string::String::new(),
            Self::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
            Self::Number(n) => format_number(*n),
            Self::String(s) => s.clone(),
        }
    }

    /// Convert to a typed YAML `Value` for **single-expression typed assignment**.
    ///
    /// Used when a `${{ expr }}` expression spans the entire value of a YAML field
    /// (no surrounding text), preserving the type for fields that can use it (e.g.
    /// `if:`). String fields coerce YAML scalars via serde_yaml automatically.
    ///
    /// For numeric values, whole numbers are represented as YAML integers so that
    /// fields expecting integer YAML scalars (e.g. `timeout:`) deserialize correctly.
    pub(super) fn to_yaml_value(&self) -> YamlValue {
        match self {
            Self::Bool(b) => YamlValue::Bool(*b),
            Self::Number(n) => {
                // Represent whole numbers as YAML integers (not floats) so that
                // fields like `timeout: ${{ inputs.seconds }}` deserialise correctly
                // via serde's integer path.
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    serde_yaml::to_value(*n as i64).unwrap_or(YamlValue::String(format_number(*n)))
                } else {
                    serde_yaml::to_value(n).unwrap_or(YamlValue::String(format_number(*n)))
                }
            }
            Self::String(s) => YamlValue::String(s.clone()),
            // Empty = undefined reference: treat as falsy empty string so that
            // `if:` sees a falsy value (empty string → Some(false)) rather than
            // Null (which deserialise_step_condition maps to None = "run normally").
            Self::Empty => YamlValue::String(std::string::String::new()),
        }
    }

    /// Serialize this value to its **JSON string** representation.
    ///
    /// - `Bool(b)` → `"true"` / `"false"` (bare JSON literals)
    /// - `Number(n)` → decimal string e.g. `"0"`, `"1.5"`
    /// - `String(s)` → JSON-quoted string e.g. `"\"hello\""`, `"\"false\""`
    /// - `Empty` → `"null"`
    pub(super) fn to_json_string(&self) -> std::string::String {
        match self {
            Self::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            Self::Number(n) => format_number(*n),
            Self::String(s) => serde_json::to_string(s).unwrap_or_else(|_| format!("{:?}", s)),
            Self::Empty => "null".to_string(),
        }
    }
}

/// Format a float for display: whole numbers without decimal point.
pub(super) fn format_number(n: f64) -> std::string::String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{}", n)
    }
}

/// Result of evaluating a single `${{ }}` span.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum EvaluatedSpan {
    /// Span was successfully evaluated to a typed value.
    Value(EvaluatedValue),
    /// Span references a namespace that is not active in the current substitution
    /// pass; the original placeholder text must be preserved for the next pass.
    ///
    /// **Invariant**: the LAST substitution pass (currently `args`) must fully resolve
    /// every deferred span. Deferral relies on the whole original `${{ … }}` placeholder
    /// being preserved verbatim. If a third namespace is ever added, ensure that namespace
    /// is processed last and resolves completely.
    Deferred,
}

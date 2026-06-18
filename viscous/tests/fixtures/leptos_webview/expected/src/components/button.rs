use leptos::*;

#[component]
pub fn Button(
    #[prop(into)] label: String,
    #[prop(into)] kind: String,
) -> impl IntoView {
    view! { <div class="button">"Button"</div> }
}

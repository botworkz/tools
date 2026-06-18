use leptos::*;

mod components;
mod routes;

pub fn mount() {
    console_error_panic_hook::set_once();
    leptos::mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    view! { <p>"hello from {{ crate_name }}"</p> }
}

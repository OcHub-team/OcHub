use super::*;
use ochub_core::application::{
    OverviewCandidateState, OverviewReachability, OverviewRouteExplanation,
};
use ochub_core::gateway::Dialect;

impl OverviewView {
    pub(super) fn render_routes(
        &self,
        snapshot: &OverviewSnapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = components::button(
            "overview-open-routes",
            t(k::OVERVIEW_OPEN_ROUTES),
            ButtonTone::Ghost,
            ButtonSize::Sm,
        )
        .on_click(cx.listener(|_, _, _, cx| cx.emit(OverviewEvent::Routes)));
        let mut section = div()
            .flex()
            .flex_col()
            .gap_3()
            .min_w_0()
            .child(self.section_heading(t(k::OVERVIEW_ROUTES), open.into_any_element()));
        let Some(routing) = &snapshot.routing.data else {
            return section.child(self.unavailable_section()).into_any_element();
        };
        if routing.routes.is_empty() {
            return section
                .child(note(t(k::OVERVIEW_ROUTES_EMPTY), false))
                .into_any_element();
        }
        let query = self.search.read(cx).content().trim().to_lowercase();
        let matching: Vec<_> = routing
            .routes
            .iter()
            .filter(|route| route_matches(route, routing, &query))
            .collect();
        // The highlighted row and inspector must always refer to the same
        // visible result, including when a filter hides the old selection.
        let selected = visible_route_selection(&matching, self.selected_route.as_deref());
        section = section.child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_3()
                .child(div().w(px(280.)).max_w_full().child(self.search.clone()))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(t(k::OVERVIEW_CONFIG_NOT_TRACE)),
                ),
        );
        let mut table = div()
            .flex()
            .flex_col()
            .min_w_0()
            .border_1()
            .border_color(theme::border())
            .rounded_lg()
            .overflow_hidden()
            .bg(theme::surface());
        if matching.is_empty() {
            table = table.child(note(t(k::OVERVIEW_NO_MATCHES), false));
        }
        for route in matching.iter().take(ROUTE_PREVIEW_LIMIT) {
            let is_selected = selected.is_some_and(|selected| selected.id == route.id);
            let route_id = route.id.clone();
            table = table.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .min_w_0()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme::border())
                    .when(is_selected, |row| row.bg(theme::accent_soft()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .truncate()
                                    .text_color(theme::text())
                                    .child(route.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .truncate()
                                    .text_color(theme::subtext())
                                    .child(tf!(
                                        k::OVERVIEW_ROUTE_COUNTS,
                                        channels = route.enabled_channel_count,
                                        rules = route.model_rule_count
                                    )),
                            ),
                    )
                    .child(components::badge(
                        BadgeTone::Neutral,
                        if route.enabled {
                            t(k::OVERVIEW_ROUTE_ENABLED)
                        } else {
                            t(k::OVERVIEW_ROUTE_DISABLED)
                        },
                    ))
                    .child(
                        components::button(
                            SharedString::from(format!("overview-route-{}", route.id)),
                            if is_selected {
                                t(k::OVERVIEW_SELECTED)
                            } else {
                                t(k::OVERVIEW_INSPECT)
                            },
                            ButtonTone::Ghost,
                            ButtonSize::Sm,
                        )
                        .aria_selected(is_selected)
                        .aria_label(format!("{} · {}", route.name, raw(k::OVERVIEW_INSPECT)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.selected_route = Some(route_id.clone());
                            this.route_binding = None;
                            this.route_binding_open = false;
                            this.show_excluded_candidates = false;
                            cx.notify();
                        })),
                    ),
            );
        }
        if matching.len() > ROUTE_PREVIEW_LIMIT {
            table = table.child(note(
                tf!(
                    k::OVERVIEW_PREVIEW_LIMIT,
                    shown = ROUTE_PREVIEW_LIMIT,
                    total = matching.len()
                )
                .into(),
                false,
            ));
        }
        if let Some(route) = selected {
            table = table.child(self.render_route_detail(route, routing, snapshot, cx));
        }
        section.child(table).into_any_element()
    }

    fn render_route_detail(
        &self,
        route: &OverviewRoute,
        routing: &OverviewRouting,
        snapshot: &OverviewSnapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut detail = div()
            .flex()
            .flex_col()
            .gap_4()
            .min_w_0()
            .p_4()
            .bg(theme::content_background())
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t(k::OVERVIEW_ROUTE_PREVIEW)),
                    )
                    .child(components::badge(
                        BadgeTone::Neutral,
                        t(k::OVERVIEW_CONFIG_PATH),
                    )),
            );
        if !routing.policy_details_available {
            return detail
                .child(note(t(k::OVERVIEW_POLICY_UNAVAILABLE), false))
                .into_any_element();
        }
        let bindings: Vec<_> = routing
            .bindings
            .iter()
            .filter(|binding| binding.route_id == route.id)
            .collect();
        let selected = bindings
            .iter()
            .position(|binding| Some(&binding.id) == self.route_binding.as_ref());
        let binding = selected.map(|index| bindings[index]);
        let mut labels = vec![raw(k::OVERVIEW_ROUTE_POLICY).to_string()];
        labels.extend(bindings.iter().map(|binding| {
            if binding.enabled {
                binding.name.clone()
            } else {
                format!("{} · {}", binding.name, raw(k::OVERVIEW_ROUTE_DISABLED))
            }
        }));
        let mut ids = vec![None];
        ids.extend(bindings.iter().map(|binding| Some(binding.id.clone())));
        let view = cx.entity();
        let select = components::select_dropdown(
            "overview-route-binding",
            &labels.iter().map(String::as_str).collect::<Vec<_>>(),
            selected.map_or(0, |index| index + 1),
            self.route_binding_open,
            move |event, _, cx| {
                view.update(cx, |this, cx| {
                    match event {
                        components::SelectDropdownEvent::Open(open) => {
                            this.route_binding_open = open
                        }
                        components::SelectDropdownEvent::Select(index) => {
                            this.route_binding = ids.get(index).cloned().flatten();
                            this.route_binding_open = false;
                        }
                    }
                    cx.notify();
                })
            },
        );
        let inlet = self.route_dialect;
        let on_dialect = cx.listener(|this, index: &usize, _, cx| {
            this.route_dialect = match index {
                1 => Dialect::Responses,
                2 => Dialect::Chat,
                _ => Dialect::Messages,
            };
            cx.notify();
        });
        detail = detail.child(
            div()
                .flex()
                .flex_wrap()
                .gap_3()
                .min_w_0()
                .child(div().flex_1().min_w(px(200.)).child(components::field(
                    t(k::OVERVIEW_CALLER_POLICY),
                    false,
                    None,
                    select,
                )))
                .child(div().flex_1().min_w(px(240.)).child(components::field(
                    t(k::OVERVIEW_REQUEST_PROTOCOL),
                    false,
                    None,
                    components::segmented(
                        "overview-route-dialect",
                        &["Messages", "Responses", "Chat"],
                        match inlet {
                            Dialect::Messages => 0,
                            Dialect::Responses => 1,
                            Dialect::Chat => 2,
                        },
                        move |index, window, cx| on_dialect(&index, window, cx),
                    ),
                ))),
        );
        detail = detail.child(components::field(
            t(k::OVERVIEW_CLIENT_MODEL),
            false,
            None,
            self.route_model.clone(),
        ));
        let suggestions =
            if let Some(policy) = binding.and_then(|binding| binding.model_policy.as_ref()) {
                policy.client_models()
            } else {
                let mut models: Vec<_> = route
                    .model_rules
                    .iter()
                    .filter(|rule| !rule.model.contains('*'))
                    .map(|rule| rule.model.clone())
                    .collect();
                if let Some(model) = &route.default_model
                    && !models.contains(model)
                {
                    models.push(model.clone());
                }
                models
            };
        if !suggestions.is_empty() {
            detail =
                detail.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme::muted())
                                .child(t(k::OVERVIEW_MODEL_TRY)),
                        )
                        .children(suggestions.into_iter().take(4).enumerate().map(
                            |(index, model)| {
                                components::button(
                                    SharedString::from(format!("overview-model-example-{index}")),
                                    model.clone(),
                                    ButtonTone::Ghost,
                                    ButtonSize::Sm,
                                )
                                .max_w_full()
                                .truncate()
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.route_model.update(cx, |input, cx| {
                                            input.set_content(model.clone(), cx)
                                        });
                                        cx.notify();
                                    },
                                ))
                            },
                        )),
                );
        }
        let model = self.route_model.read(cx).content();
        if let Some(explanation) = routing.explain(
            &route.id,
            binding.map(|binding| binding.id.as_str()),
            model.trim(),
            inlet,
        ) {
            detail = detail.child(self.render_route_explanation(&explanation, routing, cx));
        } else {
            detail = detail.child(note(t(k::OVERVIEW_ENTER_MODEL), false));
        }
        if !route.missing_channel_ids.is_empty() {
            detail = detail.child(note(
                tf!(
                    k::OVERVIEW_MISSING_CANDIDATES,
                    count = route.missing_channel_ids.len()
                )
                .into(),
                true,
            ));
        }
        let apps: Vec<_> = snapshot
            .apps
            .data
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|app| {
                app.enabled
                    && app
                        .connections
                        .iter()
                        .any(|connection| connection.gateway_route_id.as_deref() == Some(&route.id))
            })
            .collect();
        if !apps.is_empty() {
            detail = detail.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(t(k::OVERVIEW_APPLICATIONS)),
                    )
                    .children(apps.into_iter().take(APP_PREVIEW_LIMIT).filter_map(|app| {
                        let id = AppId::parse(&app.app).ok()?;
                        Some(
                            components::button(
                                SharedString::from(format!("overview-linked-app-{}", app.app)),
                                app.display_name.clone(),
                                ButtonTone::Ghost,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(
                                move |_, _, _, cx| cx.emit(OverviewEvent::App(id.clone())),
                            )),
                        )
                    })),
            );
        }
        detail
            .child(note(t(k::OVERVIEW_PREVIEW_SCOPE), false))
            .into_any_element()
    }

    fn render_route_explanation(
        &self,
        explanation: &OverviewRouteExplanation,
        routing: &OverviewRouting,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let eligible = explanation
            .candidates
            .iter()
            .filter(|candidate| candidate.state == OverviewCandidateState::Eligible)
            .count();
        let excluded = explanation.candidates.len() - eligible;
        let mapping = if let Some(rule) = &explanation.matched_rule {
            tf!(k::OVERVIEW_MATCHED_RULE, pattern = rule)
        } else if explanation.uses_fallback {
            raw(k::OVERVIEW_USED_FALLBACK).to_string()
        } else {
            raw(k::OVERVIEW_MODEL_PASSTHROUGH).to_string()
        };
        let policy = if explanation.uses_binding_policy {
            t(k::OVERVIEW_BINDING_POLICY)
        } else {
            t(k::OVERVIEW_ROUTE_POLICY)
        };
        let mut content = div()
            .flex()
            .flex_col()
            .gap_3()
            .min_w_0()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .min_w_0()
                    .child(route_stage(
                        "01",
                        t(k::OVERVIEW_CLIENT_MODEL),
                        explanation.requested_model.clone().into(),
                        raw(k::OVERVIEW_SENT_BY_CLIENT).into(),
                    ))
                    .child(route_stage(
                        "02",
                        policy,
                        explanation.routing_model.clone().into(),
                        mapping.into(),
                    ))
                    .child(route_stage(
                        "03",
                        t(k::OVERVIEW_CONFIG_CANDIDATES),
                        tf!(k::OVERVIEW_ELIGIBLE_COUNT, count = eligible).into(),
                        t(k::OVERVIEW_SELECTION_ORDER),
                    )),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(t(k::OVERVIEW_CANDIDATE_ORDER)),
            );
        if eligible == 0 {
            content = content.child(note(t(k::OVERVIEW_NO_ELIGIBLE_CANDIDATES), true));
        }
        let visible: Vec<_> = explanation
            .candidates
            .iter()
            .filter(|candidate| {
                candidate.state == OverviewCandidateState::Eligible || self.show_excluded_candidates
            })
            .collect();
        for candidate in visible.iter().take(8) {
            let Some(channel) = routing
                .channels
                .iter()
                .find(|channel| channel.id == candidate.channel_id)
            else {
                continue;
            };
            let allowed = candidate.state == OverviewCandidateState::Eligible;
            let accessibility_label = format!(
                "{} · {} · {} · {} · {}",
                channel.name,
                channel.dialect.as_str(),
                candidate.upstream_model,
                tf!(
                    k::OVERVIEW_PRIORITY_WEIGHT,
                    priority = channel.priority,
                    weight = channel.weight
                ),
                candidate_state_label(candidate.state),
            );
            content = content.child(
                div()
                    .id(SharedString::from(format!(
                        "overview-candidate-{}",
                        channel.id
                    )))
                    .role(gpui::Role::Label)
                    .aria_label(accessibility_label)
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_3()
                    .min_w_0()
                    .py_2()
                    .border_t_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(170.))
                            .child(
                                div()
                                    .truncate()
                                    .text_sm()
                                    .text_color(theme::text())
                                    .child(channel.name.clone()),
                            )
                            .child(div().truncate().text_xs().text_color(theme::muted()).child(
                                format!(
                                    "{} · {}",
                                    channel.dialect.as_str(),
                                    channel.origin.as_deref().unwrap_or("—")
                                ),
                            )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(160.))
                            .child(
                                div()
                                    .truncate()
                                    .text_sm()
                                    .text_color(theme::text())
                                    .child(candidate.upstream_model.clone()),
                            )
                            .child(div().text_xs().text_color(theme::muted()).child(tf!(
                                k::OVERVIEW_PRIORITY_WEIGHT,
                                priority = channel.priority,
                                weight = channel.weight
                            ))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_end()
                            .gap_1()
                            .child(components::badge(
                                if allowed {
                                    BadgeTone::Accent
                                } else {
                                    BadgeTone::Neutral
                                },
                                candidate_state_label(candidate.state),
                            ))
                            .child(div().text_xs().text_color(theme::muted()).child(t(
                                match channel.reachability {
                                    OverviewReachability::Unknown => {
                                        k::OVERVIEW_REACHABILITY_UNKNOWN
                                    }
                                    OverviewReachability::Reachable => {
                                        k::OVERVIEW_REACHABILITY_REACHABLE
                                    }
                                    OverviewReachability::Unreachable => {
                                        k::OVERVIEW_REACHABILITY_UNREACHABLE
                                    }
                                },
                            ))),
                    ),
            );
        }
        if visible.len() > 8 {
            content = content.child(note(
                tf!(k::OVERVIEW_PREVIEW_LIMIT, shown = 8, total = visible.len()).into(),
                false,
            ));
        }
        if excluded > 0 {
            content = content.child(
                components::button(
                    "overview-excluded",
                    if self.show_excluded_candidates {
                        t(k::OVERVIEW_HIDE_EXCLUDED)
                    } else {
                        tf!(k::OVERVIEW_SHOW_EXCLUDED, count = excluded).into()
                    },
                    ButtonTone::Ghost,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.show_excluded_candidates = !this.show_excluded_candidates;
                    cx.notify();
                })),
            );
        }
        content
            .child(note(t(k::OVERVIEW_REACHABILITY_SCOPE), false))
            .into_any_element()
    }
}

fn route_stage(
    number: &'static str,
    label: SharedString,
    value: SharedString,
    detail: SharedString,
) -> gpui::Stateful<gpui::Div> {
    let accessibility_label = format!("{number}. {label}: {value}. {detail}");
    div()
        .id(SharedString::from(format!("overview-route-stage-{number}")))
        .role(gpui::Role::Label)
        .aria_label(accessibility_label)
        .flex_1()
        .min_w(px(160.))
        .max_w_full()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(theme::border())
        .bg(theme::surface())
        .child(
            div()
                .flex()
                .gap_2()
                .text_xs()
                .text_color(theme::muted())
                .child(number)
                .child(label),
        )
        .child(
            div()
                .pt_2()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::text())
                .truncate()
                .child(value),
        )
        .child(
            div()
                .pt_1()
                .text_xs()
                .text_color(theme::muted())
                .child(detail),
        )
}

fn candidate_state_label(state: OverviewCandidateState) -> SharedString {
    use OverviewCandidateState::*;
    t(match state {
        Eligible => k::OVERVIEW_CANDIDATE_ELIGIBLE,
        RouteDisabled => k::OVERVIEW_ROUTE_DISABLED,
        BindingDisabled => k::OVERVIEW_BINDING_DISABLED,
        ChannelDisabled => k::OVERVIEW_CHANNEL_DISABLED,
        OutsideRoute => k::OVERVIEW_OUTSIDE_ROUTE,
        RuleChannelMismatch => k::OVERVIEW_RULE_CHANNEL_MISMATCH,
        RuleDialectMismatch => k::OVERVIEW_RULE_DIALECT_MISMATCH,
        UnsupportedDialect => k::OVERVIEW_UNSUPPORTED_DIALECT,
        ModelMismatch => k::OVERVIEW_MODEL_MISMATCH,
    })
}

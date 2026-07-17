use super::{
    default_bootstrap_path, load_build_config, load_test_config, validate_build_steps,
    validate_test_ports, validate_test_steps, TestConfig, TestIso, MAX_INCLUDE_DEPTH,
};
use crate::plan::files::FileEntry;
use crate::qemu::PortSpec;
use crate::resolver::Reference;
use crate::step::{
    ArchiveStep, ArchiveStepSpec, ExpectBlock, RunStep, StdioExpect, StepTarget, TestStep,
};
use std::path::PathBuf;
use tempfile::TempDir;

fn loopback(port: u16) -> PortSpec {
    PortSpec {
        addr: "127.0.0.1".into(),
        port,
    }
}

fn run_ref(step: &TestStep) -> &RunStep {
    let TestStep::Run(step) = step else {
        panic!("expected run step");
    };
    step
}

fn make_step(target: StepTarget, name: &str) -> TestStep {
    TestStep::Run(RunStep {
        target,
        name: name.to_string(),
        run: "echo ok".to_string(),
        timeout: None,
        shell: None,
        sudo: None,
        id: None,
        expect: None,
        condition: None,
    })
}

fn write_yaml(repo: &TempDir, name: &str, content: &str) -> PathBuf {
    let path = repo.path().join(name);
    std::fs::write(&path, content).unwrap();
    path
}

fn build_doc(
    repo: &TempDir,
    file_name: &str,
    name: &str,
    image: &str,
    output: &str,
    body: &str,
) -> PathBuf {
    write_yaml(
        repo,
        file_name,
        &format!(
            "type: botforge/build\nname: {name}\nimage: \"{image}\"\noutput: \"{output}\"\n{body}"
        ),
    )
}

fn test_doc(repo: &TempDir, file_name: &str, name: &str, body: &str) -> PathBuf {
    write_yaml(
        repo,
        file_name,
        &format!("type: botforge/test\nname: {name}\n{body}"),
    )
}

fn write_build_config(repo: &TempDir, name: &str, content: &str) {
    write_yaml(repo, name, content);
}

fn write_test_config(repo: &TempDir, name: &str, content: &str) {
    write_yaml(repo, name, content);
}

mod ports {
    use super::*;

    #[test]
    fn test_config_isos_parses_legacy_and_bootstrap_shapes() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
isos:
  - some/legacy.iso
  - path: some/payload.iso
    label: botwork-payload
    mount: /mnt/botwork-payload
"#,
        )
        .unwrap();

        assert_eq!(config.isos.len(), 2);
        match &config.isos[0] {
            TestIso::Attach(path) => assert_eq!(path, &PathBuf::from("some/legacy.iso")),
            TestIso::Bootstrap { .. } => panic!("expected legacy iso entry"),
        }
        match &config.isos[1] {
            TestIso::Bootstrap {
                path,
                label,
                mount,
                bootstrap,
            } => {
                assert_eq!(path, &PathBuf::from("some/payload.iso"));
                assert_eq!(label, "botwork-payload");
                assert_eq!(mount, &PathBuf::from("/mnt/botwork-payload"));
                assert_eq!(bootstrap, &default_bootstrap_path());
            }
            TestIso::Attach(_) => panic!("expected bootstrap iso entry"),
        }
    }

    #[test]
    fn test_config_isos_parses_bootstrap_override() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
isos:
  - path: other.iso
    label: lbl
    mount: /mnt/other
    bootstrap: custom-init.sh
"#,
        )
        .unwrap();

        match &config.isos[0] {
            TestIso::Bootstrap { bootstrap, .. } => {
                assert_eq!(bootstrap, &PathBuf::from("custom-init.sh"))
            }
            TestIso::Attach(_) => panic!("expected bootstrap iso entry"),
        }
    }

    #[test]
    fn test_config_isos_parses_empty_list() {
        let config: TestConfig = serde_yaml::from_str("isos: []\n").unwrap();
        assert!(config.isos.is_empty());
    }

    #[test]
    fn test_config_ports_integer_parses_to_loopback() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
ports:
  - 80
"#,
        )
        .unwrap();
        assert_eq!(config.ports.len(), 1);
        assert_eq!(config.ports[0], loopback(80));
    }

    #[test]
    fn test_config_ports_string_parses_to_custom_addr() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
ports:
  - "0.0.0.0:9901"
"#,
        )
        .unwrap();
        assert_eq!(config.ports.len(), 1);
        assert_eq!(
            config.ports[0],
            PortSpec {
                addr: "0.0.0.0".into(),
                port: 9901
            }
        );
    }

    #[test]
    fn test_config_ports_explicit_loopback_string() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
ports:
  - "127.0.0.1:80"
"#,
        )
        .unwrap();
        assert_eq!(config.ports[0], loopback(80));
    }

    #[test]
    fn test_config_ports_mixed_int_and_string() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
ports:
  - 80
  - "0.0.0.0:9901"
"#,
        )
        .unwrap();
        assert_eq!(config.ports.len(), 2);
        assert_eq!(config.ports[0], loopback(80));
        assert_eq!(
            config.ports[1],
            PortSpec {
                addr: "0.0.0.0".into(),
                port: 9901
            }
        );
    }

    #[test]
    fn test_config_ports_default_is_empty() {
        let config: TestConfig = serde_yaml::from_str("steps: []\n").unwrap();
        assert!(config.ports.is_empty());
    }

    #[test]
    fn test_config_ports_malformed_string_rejected() {
        assert!(serde_yaml::from_str::<TestConfig>("ports:\n  - \"noport\"\n").is_err());
        assert!(serde_yaml::from_str::<TestConfig>("ports:\n  - \":80\"\n").is_err());
        assert!(
            serde_yaml::from_str::<TestConfig>("ports:\n  - \"0.0.0.0:notanumber\"\n").is_err()
        );
        assert!(serde_yaml::from_str::<TestConfig>("ports:\n  - \"0.0.0.0:99999\"\n").is_err());
    }

    #[test]
    fn test_config_ports_validation_rejects_invalid_and_duplicate_values() {
        assert!(validate_test_ports(&[loopback(0)], 2222).is_err());
        assert!(validate_test_ports(&[loopback(2222)], 2222).is_err());
        assert!(validate_test_ports(&[loopback(22)], 2222).is_err());
        assert!(validate_test_ports(&[loopback(80), loopback(80)], 2222).is_err());
        // duplicate port number regardless of address
        assert!(validate_test_ports(
            &[
                loopback(80),
                PortSpec {
                    addr: "0.0.0.0".into(),
                    port: 80
                }
            ],
            2222
        )
        .is_err());
    }

    #[test]
    fn test_config_ports_validation_accepts_distinct_ports() {
        assert!(validate_test_ports(
            &[
                loopback(80),
                PortSpec {
                    addr: "0.0.0.0".into(),
                    port: 9901
                }
            ],
            2222
        )
        .is_ok());
    }
}

mod steps {
    use super::*;

    // --- step deserialization ---

    #[test]
    fn test_step_parses_guest_step() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
steps:
  - on: guest
    name: goss
    run: goss -g /path/goss.yaml validate
"#,
        )
        .unwrap();

        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).target, StepTarget::Guest);
        assert_eq!(run_ref(&config.steps[0]).name, "goss");
        assert_eq!(
            run_ref(&config.steps[0]).run,
            "goss -g /path/goss.yaml validate"
        );
    }

    #[test]
    fn test_step_parses_host_step() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
ports:
  - 80
steps:
  - on: host
    name: vm-narrative
    run: bash smoke/vm-narrative.sh 127.0.0.1
"#,
        )
        .unwrap();

        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).target, StepTarget::Host);
        assert_eq!(run_ref(&config.steps[0]).name, "vm-narrative");
        assert_eq!(
            run_ref(&config.steps[0]).run,
            "bash smoke/vm-narrative.sh 127.0.0.1"
        );
    }

    #[test]
    fn test_step_parses_timeout_seconds() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
steps:
  - on: guest
    name: long-step
    timeout: 900
    run: echo hello
"#,
        )
        .unwrap();

        assert_eq!(run_ref(&config.steps[0]).timeout, Some(900));
    }

    #[test]
    fn test_step_parses_interleaved_guest_and_host_steps_in_order() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
ports:
  - 80
steps:
  - on: guest
    name: goss
    run: goss -g /path/goss.yaml validate
  - on: guest
    name: flip-spigot
    run: sudo cp /etc/envoy/rds/active.ingress.yaml /etc/envoy/rds/active.yaml
  - on: host
    name: vm-narrative
    run: bash smoke/vm-narrative.sh 127.0.0.1
  - on: guest
    name: flip-spigot-back
    run: sudo cp /etc/envoy/rds/active.holding.yaml /etc/envoy/rds/active.yaml
"#,
        )
        .unwrap();

        assert_eq!(config.steps.len(), 4);
        assert_eq!(run_ref(&config.steps[0]).target, StepTarget::Guest);
        assert_eq!(run_ref(&config.steps[1]).target, StepTarget::Guest);
        assert_eq!(run_ref(&config.steps[2]).target, StepTarget::Host);
        assert_eq!(run_ref(&config.steps[3]).target, StepTarget::Guest);
    }

    #[test]
    fn test_step_parses_missing_on_field_as_guest() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
steps:
  - name: no-on-field
    run: echo hello
"#,
        )
        .unwrap();
        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).target, StepTarget::Guest);
    }

    #[test]
    fn test_step_rejects_invalid_on_value() {
        let result: Result<TestConfig, _> = serde_yaml::from_str(
            r#"
steps:
  - on: invalid
    name: bad-step
    run: echo hello
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_load_test_config_expands_uses_steps_with_inputs() {
        let repo = TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join("shared")).unwrap();
        std::fs::write(
            repo.path().join("shared/narrative.yaml"),
            r#"
type: botforge/fragment
inputs:
  target:
    type: string
    required: true
  shell:
    type: string
    default: bash
steps:
  - on: guest
    name: "narrative-${{ inputs.target }}"
    shell: ${{ inputs.shell }}
    run: |
      echo "${USER}"
      bash /tmp/${{ inputs.target }}.sh
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://shared/narrative.yaml"
    with:
      target: edge
      shell: bash
"#,
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();

        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).name, "narrative-edge");
        assert_eq!(run_ref(&config.steps[0]).shell.as_deref(), Some("bash"));
        assert!(run_ref(&config.steps[0]).run.contains(r#"echo "${USER}""#));
        assert!(run_ref(&config.steps[0]).run.contains("bash /tmp/edge.sh"));
    }

    #[test]
    fn test_load_test_config_preserves_fragment_sudo_via_uses() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/fragment
steps:
  - on: guest
    name: frag-root-step
    sudo: true
    run: echo from-fragment
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://frag.yaml"
"#,
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();

        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).name, "frag-root-step");
        assert_eq!(run_ref(&config.steps[0]).sudo, Some(true));
    }

    #[test]
    fn test_load_test_config_expands_step_level_for_scalar_items() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - name: "check-${{ args.0 }}"
    for: [auth-broker, api]
    run: echo ${{ args.0 }}
"#,
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(config.steps.len(), 2);
        assert_eq!(run_ref(&config.steps[0]).name, "check-auth-broker");
        assert_eq!(run_ref(&config.steps[0]).run, "echo auth-broker");
        assert_eq!(run_ref(&config.steps[1]).name, "check-api");
        assert_eq!(run_ref(&config.steps[1]).run, "echo api");
    }

    #[test]
    fn test_load_test_config_expands_step_level_for_sequence_items() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - name: "check-${{ args.0 }}"
    for:
      - [foo, foo-svc]
      - [bar, bar-svc]
    run: echo ${{ args.0 }} ${{ args.1 }}
"#,
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(config.steps.len(), 2);
        assert_eq!(run_ref(&config.steps[0]).name, "check-foo");
        assert_eq!(run_ref(&config.steps[0]).run, "echo foo foo-svc");
        assert_eq!(run_ref(&config.steps[1]).name, "check-bar");
        assert_eq!(run_ref(&config.steps[1]).run, "echo bar bar-svc");
    }

    #[test]
    fn test_load_test_config_expands_step_level_for_assoc_items() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - name: "check-${{ args.label }}"
    for:
      - { label: foo, svc: foo-svc }
      - { label: bar, svc: bar-svc }
    run: echo ${{ args.svc }}
"#,
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(config.steps.len(), 2);
        assert_eq!(run_ref(&config.steps[0]).name, "check-foo");
        assert_eq!(run_ref(&config.steps[0]).run, "echo foo-svc");
        assert_eq!(run_ref(&config.steps[1]).name, "check-bar");
        assert_eq!(run_ref(&config.steps[1]).run, "echo bar-svc");
    }

    #[test]
    fn test_load_test_config_step_level_for_preserves_expect_block() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - name: check-${{ args.0 }}
    for: [alpha, beta]
    run: echo ${{ args.0 }}
    expect:
      stdout:
        contains:
          - ${{ args.0 }}
"#,
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(config.steps.len(), 2);
        assert_eq!(
            run_ref(&config.steps[0])
                .expect
                .as_ref()
                .unwrap()
                .stdout
                .as_ref()
                .unwrap()
                .contains,
            vec!["alpha".to_string()]
        );
        assert_eq!(
            run_ref(&config.steps[1])
                .expect
                .as_ref()
                .unwrap()
                .stdout
                .as_ref()
                .unwrap()
                .contains,
            vec!["beta".to_string()]
        );
    }

    #[test]
    fn test_load_test_config_step_level_for_interops_with_uses() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/fragment
steps:
  - on: guest
    name: frag-step
    run: echo from-fragment
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - name: check-${{ args.0 }}
    for: [one, two]
    run: echo ${{ args.0 }}
  - uses: "@://frag.yaml"
"#,
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(config.steps.len(), 3);
        assert_eq!(run_ref(&config.steps[0]).name, "check-one");
        assert_eq!(run_ref(&config.steps[1]).name, "check-two");
        assert_eq!(run_ref(&config.steps[2]).name, "frag-step");
    }

    #[test]
    fn test_load_test_config_expands_fragment_scalar_for_via_uses() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/fragment
steps:
  - name: "frag-${{ args.0 }}"
    for: [alpha, beta]
    run: echo ${{ args.0 }}
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();

        assert_eq!(config.steps.len(), 2);
        assert_eq!(run_ref(&config.steps[0]).name, "frag-alpha");
        assert_eq!(run_ref(&config.steps[0]).run, "echo alpha");
        assert_eq!(run_ref(&config.steps[1]).name, "frag-beta");
        assert_eq!(run_ref(&config.steps[1]).run, "echo beta");
    }

    #[test]
    fn test_load_test_config_expands_fragment_sequence_for_via_uses() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/fragment
steps:
  - name: "pair-${{ args.0 }}"
    for:
      - [cat, /usr/bin/cat]
      - [ls, /usr/bin/ls]
    run: echo ${{ args.0 }} ${{ args.1 }}
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();

        assert_eq!(config.steps.len(), 2);
        assert_eq!(run_ref(&config.steps[0]).name, "pair-cat");
        assert_eq!(run_ref(&config.steps[0]).run, "echo cat /usr/bin/cat");
        assert_eq!(run_ref(&config.steps[1]).name, "pair-ls");
        assert_eq!(run_ref(&config.steps[1]).run, "echo ls /usr/bin/ls");
    }

    #[test]
    fn test_load_test_config_expands_fragment_mapping_for_via_uses() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/fragment
steps:
  - name: "svc-${{ args.name }}"
    for:
      - { name: coreutils-cat, bin: cat }
      - { name: coreutils-ls, bin: ls }
    run: echo ${{ args.bin }}
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();

        assert_eq!(config.steps.len(), 2);
        assert_eq!(run_ref(&config.steps[0]).name, "svc-coreutils-cat");
        assert_eq!(run_ref(&config.steps[0]).run, "echo cat");
        assert_eq!(run_ref(&config.steps[1]).name, "svc-coreutils-ls");
        assert_eq!(run_ref(&config.steps[1]).run, "echo ls");
    }

    #[test]
    fn test_load_test_config_expands_fragment_mixed_inputs_and_args_namespaces() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/fragment
inputs:
  svc:
    type: string
    required: true
steps:
  - name: "${{ inputs.svc }}-${{ args.0 }}"
    for: [cp, ls]
    run: echo ${{ inputs.svc }} ${{ args.0 }}
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://frag.yaml"
    with:
      svc: api
"#,
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();

        assert_eq!(config.steps.len(), 2);
        assert_eq!(run_ref(&config.steps[0]).name, "api-cp");
        assert_eq!(run_ref(&config.steps[0]).run, "echo api cp");
        assert_eq!(run_ref(&config.steps[1]).name, "api-ls");
        assert_eq!(run_ref(&config.steps[1]).run, "echo api ls");
    }

    #[test]
    fn test_load_test_config_fragment_missing_active_namespace_input_still_errors() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/fragment
inputs:
  svc:
    type: string
    required: true
steps:
  - name: broken
    run: echo ${{ inputs.typo }}
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://frag.yaml"
    with:
      svc: api
"#,
        )
        .unwrap();

        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("missing required input 'typo'"));
    }

    #[test]
    fn test_load_test_config_fragment_unknown_namespace_still_errors() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/fragment
steps:
  - name: broken
    run: echo ${{ bogus.x }}
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();

        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unsupported expression"));
        assert!(msg.contains("bogus.x"));
    }

    #[test]
    fn test_load_test_config_fragment_for_expect_is_cloned_and_substituted() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/fragment
steps:
  - name: "check-${{ args.0 }}"
    for: [alpha, beta]
    run: echo ${{ args.0 }}
    expect:
      stdout:
        contains:
          - ${{ args.0 }}
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();

        assert_eq!(config.steps.len(), 2);
        assert_eq!(
            run_ref(&config.steps[0])
                .expect
                .as_ref()
                .unwrap()
                .stdout
                .as_ref()
                .unwrap()
                .contains,
            vec!["alpha".to_string()]
        );
        assert_eq!(
            run_ref(&config.steps[1])
                .expect
                .as_ref()
                .unwrap()
                .stdout
                .as_ref()
                .unwrap()
                .contains,
            vec!["beta".to_string()]
        );
    }

    #[test]
    fn test_load_test_config_rejects_unsupported_uses_scheme() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - uses: "file://shared/narrative.yaml"
"#,
        )
        .unwrap();

        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("unsupported uses scheme 'file'"));
    }

    #[test]
    fn test_load_test_config_rejects_missing_include_input() {
        let repo = TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join("shared")).unwrap();
        std::fs::write(
            repo.path().join("shared/narrative.yaml"),
            r#"
type: botforge/fragment
inputs:
  target:
    type: string
    required: true
steps:
  - on: guest
    name: "${{ inputs.target }}"
    run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://shared/narrative.yaml"
"#,
        )
        .unwrap();

        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("missing required input 'target'"));
    }

    #[test]
    fn test_load_test_config_rejects_bare_list_fragment() {
        let repo = TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join("shared")).unwrap();
        std::fs::write(
            repo.path().join("shared/narrative.yaml"),
            r#"
- on: guest
  name: bare
  run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://shared/narrative.yaml"
"#,
        )
        .unwrap();

        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(
            format!("{err:#}").contains("must be a mapping with a 'steps:' key"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn test_load_test_config_rejects_parent_segments_in_uses_path() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://shared/../narrative.yaml"
"#,
        )
        .unwrap();

        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("must contain no '.' or '..' segments"));
    }

    // --- step validation ---

    #[test]
    fn test_validate_steps_accepts_host_step() {
        let steps = vec![make_step(StepTarget::Host, "s")];
        assert!(validate_test_steps(&steps, &[loopback(80)]).is_ok());
    }

    #[test]
    fn test_validate_steps_rejects_host_step_without_ports() {
        let steps = vec![make_step(StepTarget::Host, "edge")];
        let err = validate_test_steps(&steps, &[]).unwrap_err();
        assert!(
            err.to_string().contains("ports"),
            "error should mention 'ports': {err}"
        );
    }

    #[test]
    fn test_validate_steps_accepts_empty_steps_without_ports() {
        assert!(validate_test_steps(&[], &[]).is_ok());
    }

    #[test]
    fn test_validate_steps_accepts_guest_only_without_ports() {
        let steps = vec![make_step(StepTarget::Guest, "s")];
        assert!(validate_test_steps(&steps, &[]).is_ok());
    }

    #[test]
    fn test_validate_steps_rejects_host_step_with_sudo() {
        let mut step = make_step(StepTarget::Host, "host-root");
        let TestStep::Run(run) = &mut step else {
            panic!("expected run step");
        };
        run.sudo = Some(true);
        let err = validate_test_steps(&[step], &[loopback(80)]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("host-root"),
            "error should mention step name: {msg}"
        );
        assert!(msg.contains("sudo"), "error should mention sudo: {msg}");
        assert!(msg.contains("guest"), "error should mention guest: {msg}");
    }

    #[test]
    fn test_validate_steps_accepts_host_step_with_explicit_sudo_false() {
        let mut step = make_step(StepTarget::Host, "host-unprivileged");
        let TestStep::Run(run) = &mut step else {
            panic!("expected run step");
        };
        run.sudo = Some(false);
        assert!(validate_test_steps(&[step], &[loopback(80)]).is_ok());
    }

    #[test]
    fn test_validate_steps_accepts_guest_step_with_sudo() {
        let mut step = make_step(StepTarget::Guest, "guest-root");
        let TestStep::Run(run) = &mut step else {
            panic!("expected run step");
        };
        run.sudo = Some(true);
        assert!(validate_test_steps(&[step], &[]).is_ok());
    }

    // --- shell deserialization ---

    #[test]
    fn test_step_parses_shell_python() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
steps:
  - on: guest
    name: py-step
    shell: python
    run: print("hello")
"#,
        )
        .unwrap();
        assert_eq!(run_ref(&config.steps[0]).shell.as_deref(), Some("python"));
    }

    #[test]
    fn test_step_parses_without_shell_defaults_to_none() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
steps:
  - on: guest
    name: no-shell
    run: echo hello
"#,
        )
        .unwrap();
        assert!(run_ref(&config.steps[0]).shell.is_none());
    }

    #[test]
    fn test_validate_steps_rejects_bad_shell() {
        let mut step = make_step(StepTarget::Guest, "bad-shell");
        let TestStep::Run(run) = &mut step else {
            panic!("expected run step");
        };
        run.shell = Some("fish".to_string());
        let err = validate_test_steps(&[step], &[]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fish"),
            "error should mention shell name: {msg}"
        );
    }

    // --- id field deserialization ---

    #[test]
    fn test_step_parses_id_field() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
steps:
  - on: guest
    name: my-step
    id: my-step
    run: echo hello
"#,
        )
        .unwrap();
        assert_eq!(run_ref(&config.steps[0]).id.as_deref(), Some("my-step"));
    }

    #[test]
    fn test_step_without_id_defaults_to_none() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
steps:
  - on: guest
    name: no-id-step
    run: echo hello
"#,
        )
        .unwrap();
        assert!(run_ref(&config.steps[0]).id.is_none());
    }

    #[test]
    fn test_step_unknown_field_still_errors() {
        let err = serde_yaml::from_str::<TestConfig>(
            r#"
steps:
  - on: guest
    name: my-step
    run: echo hello
    bogus_field: not-allowed
"#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bogus_field") || msg.contains("unknown field"),
            "error should mention the unknown field: {msg}"
        );
    }

    #[test]
    fn test_step_id_flows_through_uses_fragment() {
        let repo = TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join("shared")).unwrap();
        std::fs::write(
            repo.path().join("shared/frag.yaml"),
            r#"
type: botforge/fragment
steps:
  - on: guest
    name: frag-step
    id: my-frag-id
    run: echo hello
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://shared/frag.yaml"
"#,
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();

        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).name, "frag-step");
        assert_eq!(
            run_ref(&config.steps[0]).id.as_deref(),
            Some("my-frag-id"),
            "id should be preserved through fragment splice"
        );
    }
}

mod fragments {
    use super::*;

    // --- type discriminator on root documents ---

    #[test]
    fn test_load_test_config_requires_type_field() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
steps: []
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("type"), "error must mention 'type': {msg}");
    }

    #[test]
    fn test_load_test_config_rejects_unknown_type() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: unknown
steps: []
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown"),
            "error must mention the bad type value: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_rejects_fragment_as_root() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/fragment
steps: []
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("botforge test requires a 'type: botforge/test' document")
                && msg.contains("fragment"),
            "unexpected error: {msg}"
        );
    }

    // --- type discriminator on fragment documents ---

    #[test]
    fn test_load_test_config_uses_requires_type_field_on_fragment() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
steps:
  - on: guest
    name: step
    run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://frag.yaml"
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("missing required 'type:' field") || msg.contains("type"),
            "error must mention missing 'type:': {msg}"
        );
    }

    #[test]
    fn test_load_test_config_uses_rejects_entrypoint_document_as_fragment() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - on: guest
    name: step
    run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://frag.yaml"
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not a consumable fragment") && msg.contains("test"),
            "unexpected error: {msg}"
        );
    }

    // --- per-kind presence validation ---

    #[test]
    fn test_fragment_with_ports_is_rejected() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/fragment
ports:
  - 80
steps:
  - on: guest
    name: step
    run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://frag.yaml"
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ports") && msg.contains("fragment"),
            "error must mention 'ports' and 'fragment': {msg}"
        );
    }

    #[test]
    fn test_fragment_with_isos_is_rejected() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/fragment
isos:
  - some/payload.iso
steps:
  - on: guest
    name: step
    run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://frag.yaml"
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("isos") && msg.contains("fragment"),
            "error must mention 'isos' and 'fragment': {msg}"
        );
    }

    #[test]
    fn test_fragment_with_diagnostics_units_is_rejected() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/fragment
diagnostics_units:
  - some-service.service
steps:
  - on: guest
    name: step
    run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://frag.yaml"
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("diagnostics_units") && msg.contains("fragment"),
            "error must mention 'diagnostics_units' and 'fragment': {msg}"
        );
    }

    #[test]
    fn test_type_test_with_all_entrypoint_sections_loads() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
isos:
  - some/payload.iso
ports:
  - 80
diagnostics_units:
  - myservice.service
steps:
  - on: guest
    name: basic
    run: "echo ok"
"#,
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(config.isos.len(), 1);
        assert_eq!(config.ports.len(), 1);
        assert_eq!(config.diagnostics_units.len(), 1);
        assert_eq!(config.steps.len(), 1);
    }

    // --- recursion: cycle, re-entry, max depth ---

    #[test]
    fn test_load_test_config_cyclic_include_errors() {
        // root → frag_a → frag_b → frag_a  (cycle through two fragments)
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag_a.yaml"),
            r#"
type: botforge/fragment
steps:
  - uses: "@://frag_b.yaml"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("frag_b.yaml"),
            r#"
type: botforge/fragment
steps:
  - uses: "@://frag_a.yaml"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://frag_a.yaml"
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cyclic test step include detected"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_root_includes_self_cycle_errors() {
        // root → root (direct self-cycle)
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://test.yaml"
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        // The root is a type: botforge/test document, so the fragment type check fires first.
        // Either "cyclic" or "not a consumable fragment" is an acceptable error here —
        // both prevent the self-include.
        assert!(
            msg.contains("cyclic") || msg.contains("not a consumable fragment"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_reentrant_include_succeeds_and_expands_twice() {
        // Including the same fragment from two independent steps (not a cycle).
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/fragment
steps:
  - on: guest
    name: reused-step
    run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://frag.yaml"
  - uses: "@://frag.yaml"
"#,
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(
            config.steps.len(),
            2,
            "same fragment included twice must expand to two steps"
        );
        assert_eq!(run_ref(&config.steps[0]).name, "reused-step");
        assert_eq!(run_ref(&config.steps[1]).name, "reused-step");
    }

    #[test]
    fn test_load_test_config_max_depth_exceeded_errors() {
        // Create a chain of MAX_INCLUDE_DEPTH fragments deep, which should trigger the
        // depth-limit error.  With the root document seeded into the stack the limit is
        // MAX_INCLUDE_DEPTH total entries, meaning MAX_INCLUDE_DEPTH - 1 fragment levels
        // below the root.  We create exactly that many chain links plus one extra to
        // ensure the limit fires.
        let repo = TempDir::new().unwrap();
        let depth = MAX_INCLUDE_DEPTH; // 32
                                       // Each fragment 0..depth-2 includes the next one.
                                       // Fragment depth-1 is the one we try to include when the stack is full.
        for i in 0..(depth - 1) {
            let name = format!("frag{i:02}.yaml");
            let next = format!("frag{:02}.yaml", i + 1);
            std::fs::write(
                repo.path().join(&name),
                format!("type: botforge/fragment\nsteps:\n  - uses: \"@://{next}\"\n"),
            )
            .unwrap();
        }
        // The deepest fragment (depth-1) doesn't need to exist; the depth check fires
        // before loading it.  Write it anyway as a leaf so the test is self-contained.
        std::fs::write(
            repo.path().join(format!("frag{:02}.yaml", depth - 1)),
            "type: botforge/fragment\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nsteps:\n  - uses: \"@://frag00.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("depth limit") && msg.contains(&depth.to_string()),
            "error must mention the depth limit: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_fragment_contributes_file() {
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "frag.yaml",
            r#"
type: botforge/fragment
files:
  - src: "@://payload/file.txt"
    dest: /tmp/file.txt
steps: []
"#,
        );
        let test_path = test_doc(
            &repo,
            "test.yaml",
            "test",
            "steps:\n  - uses: \"@://frag.yaml\"\n",
        );
        let config = load_test_config(repo.path(), &test_path).unwrap();
        assert_eq!(
            config.files,
            vec![FileEntry {
                src: "@://payload/file.txt".to_string(),
                dest: "/tmp/file.txt".to_string(),
                ..Default::default()
            }]
        );
    }

    #[test]
    fn test_load_build_config_fragment_contributes_file() {
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "frag.yaml",
            r#"
type: botforge/fragment
files:
  - src: "@://payload/build.txt"
    dest: /tmp/build.txt
steps: []
"#,
        );
        let build_path = build_doc(
            &repo,
            "build.yaml",
            "build",
            "@base",
            "out.qcow2",
            "steps:\n  - uses: \"@://frag.yaml\"\n",
        );
        let config = load_build_config(repo.path(), &build_path).unwrap();
        assert_eq!(
            config.files,
            vec![FileEntry {
                src: "@://payload/build.txt".to_string(),
                dest: "/tmp/build.txt".to_string(),
                ..Default::default()
            }]
        );
    }

    #[test]
    fn test_load_test_config_fragment_file_walk_order_root_then_includes() {
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "frag-a.yaml",
            r#"
type: botforge/fragment
files:
  - src: "@://frag/a.txt"
    dest: /tmp/frag-a.txt
steps: []
"#,
        );
        write_test_config(
            &repo,
            "frag-b.yaml",
            r#"
type: botforge/fragment
files:
  - src: "@://frag/b.txt"
    dest: /tmp/frag-b.txt
steps: []
"#,
        );
        write_test_config(
            &repo,
            "test.yaml",
            r#"
type: botforge/test
name: test
files:
  - src: "@://root/first.txt"
    dest: /tmp/root-first.txt
  - src: "@://root/second.txt"
    dest: /tmp/root-second.txt
steps:
  - uses: "@://frag-b.yaml"
  - uses: "@://frag-a.yaml"
"#,
        );
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(
            config.files,
            vec![
                FileEntry {
                    src: "@://root/first.txt".to_string(),
                    dest: "/tmp/root-first.txt".to_string(),
                    ..Default::default()
                },
                FileEntry {
                    src: "@://root/second.txt".to_string(),
                    dest: "/tmp/root-second.txt".to_string(),
                    ..Default::default()
                },
                FileEntry {
                    src: "@://frag/b.txt".to_string(),
                    dest: "/tmp/frag-b.txt".to_string(),
                    ..Default::default()
                },
                FileEntry {
                    src: "@://frag/a.txt".to_string(),
                    dest: "/tmp/frag-a.txt".to_string(),
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn test_load_test_config_fragment_file_nested_order_is_deterministic() {
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "frag-b.yaml",
            r#"
type: botforge/fragment
files:
  - src: "@://nested/b.txt"
    dest: /tmp/nested-b.txt
steps: []
"#,
        );
        write_test_config(
            &repo,
            "frag-a.yaml",
            r#"
type: botforge/fragment
files:
  - src: "@://nested/a.txt"
    dest: /tmp/nested-a.txt
steps:
  - uses: "@://frag-b.yaml"
"#,
        );
        write_test_config(
            &repo,
            "test.yaml",
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://frag-a.yaml"
"#,
        );
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(
            config.files,
            vec![
                FileEntry {
                    src: "@://nested/a.txt".to_string(),
                    dest: "/tmp/nested-a.txt".to_string(),
                    ..Default::default()
                },
                FileEntry {
                    src: "@://nested/b.txt".to_string(),
                    dest: "/tmp/nested-b.txt".to_string(),
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn test_load_test_config_fragment_files_dedupe_identicals() {
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "frag-a.yaml",
            r#"
type: botforge/fragment
files:
  - src: "@://same/file.txt"
    dest: /tmp/same.txt
steps: []
"#,
        );
        write_test_config(
            &repo,
            "frag-b.yaml",
            r#"
type: botforge/fragment
files:
  - src: "@://same/file.txt"
    dest: /tmp/same.txt
steps: []
"#,
        );
        write_test_config(
            &repo,
            "test.yaml",
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://frag-a.yaml"
  - uses: "@://frag-b.yaml"
"#,
        );
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(
            config.files,
            vec![FileEntry {
                src: "@://same/file.txt".to_string(),
                dest: "/tmp/same.txt".to_string(),
                ..Default::default()
            }]
        );
    }

    #[test]
    fn test_load_test_config_fragment_files_non_identical_same_dest_not_deduped() {
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "frag-a.yaml",
            r#"
type: botforge/fragment
files:
  - src: "@://same/file.txt"
    dest: /tmp/same.txt
    mode: "0644"
steps: []
"#,
        );
        write_test_config(
            &repo,
            "frag-b.yaml",
            r#"
type: botforge/fragment
files:
  - src: "@://same/file.txt"
    dest: /tmp/same.txt
    mode: "0755"
steps: []
"#,
        );
        write_test_config(
            &repo,
            "test.yaml",
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://frag-a.yaml"
  - uses: "@://frag-b.yaml"
"#,
        );
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(
            config.files,
            vec![
                FileEntry {
                    src: "@://same/file.txt".to_string(),
                    dest: "/tmp/same.txt".to_string(),
                    mode: Some("0644".to_string()),
                    ..Default::default()
                },
                FileEntry {
                    src: "@://same/file.txt".to_string(),
                    dest: "/tmp/same.txt".to_string(),
                    mode: Some("0755".to_string()),
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn test_load_test_config_fragment_file_validation_matches_top_level() {
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "frag.yaml",
            r#"
type: botforge/fragment
files:
  - src: payload/file.txt
    dest: /tmp/file.txt
steps: []
"#,
        );
        write_test_config(
            &repo,
            "test.yaml",
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://frag.yaml"
"#,
        );
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("`src` must be an `@`-reference"),
            "fragment file should be validated using top-level file rules: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_fragment_file_unknown_field_rejected() {
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "frag.yaml",
            r#"
type: botforge/fragment
files:
  - src: "@://payload/file.txt"
    dest: /tmp/file.txt
    bogus: true
steps: []
"#,
        );
        write_test_config(
            &repo,
            "test.yaml",
            r#"
type: botforge/test
name: test
steps:
  - uses: "@://frag.yaml"
"#,
        );
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown field") && msg.contains("bogus"),
            "fragment file unknown fields should be rejected at parse time: {msg}"
        );
    }
}

mod loaders {
    use super::*;

    // --- BuildConfig loading ---

    #[test]
    fn test_load_build_config_minimal() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@debian-base"
output: "built.qcow2"
steps:
  - on: guest
    name: provision
    run: echo hello
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(
            config.image,
            Reference::Asset {
                name: "debian-base".to_string(),
                path: None
            }
        );
        assert_eq!(config.output, "built.qcow2");
        assert_eq!(config.disk_size, "10G");
        assert_eq!(config.step_timeout, 1800);
        assert_eq!(config.timeout, 7200);
        assert_eq!(config.cloud_init_timeout, 600);
        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).name, "provision");
    }

    #[test]
    fn test_load_build_config_overrides_defaults() {
        let repo = TempDir::new().unwrap();
        let build_path = build_doc(
            &repo,
            "build.yaml",
            "build",
            "@my-base",
            "out.qcow2",
            "disk_size: \"20G\"\nstep_timeout: 2400\ntimeout: 9600\nsteps: []\n",
        );
        let config = load_build_config(repo.path(), &build_path).unwrap();
        assert_eq!(
            config.image,
            Reference::Asset {
                name: "my-base".to_string(),
                path: None
            }
        );
        assert_eq!(config.disk_size, "20G");
        assert_eq!(config.step_timeout, 2400);
        assert_eq!(config.timeout, 9600);
        assert!(config.steps.is_empty());
        assert!(
            config.cloud_init.is_none(),
            "cloud_init should default to None"
        );
    }

    #[test]
    fn test_load_build_config_accepts_repo_traversal_image() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: botforge/build\nname: build\nimage: >-\n  @://build/artifact/foo.qcow2\noutput: \"out.qcow2\"\nsteps: []\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(
            config.image,
            Reference::Repo {
                path: Some(PathBuf::from("build/artifact/foo.qcow2"))
            }
        );
    }

    #[test]
    fn test_load_build_config_accepts_artifact_traversal_image() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: botforge/build\nname: build\nimage: \"@artifact://foo.qcow2\"\noutput: \"out.qcow2\"\nsteps: []\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(
            config.image,
            Reference::Artifact {
                path: Some(PathBuf::from("foo.qcow2"))
            }
        );
    }

    #[test]
    fn test_load_build_config_rejects_memsize_section() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: botforge/build\nname: build\nimage: \"@base\"\nmemsize: 8192\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("memsize") && msg.contains("type: botforge/build"),
            "error should mention memsize and document type: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_smp_section() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: botforge/build\nname: build\nimage: \"@base\"\nsmp: 8\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("smp") && msg.contains("type: botforge/build"),
            "error should mention smp and document type: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_files_absent_is_empty() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert!(config.files.is_empty(), "files should default to empty");
    }

    #[test]
    fn test_load_build_config_parses_top_level_files() {
        let repo = TempDir::new().unwrap();
        let build_path = build_doc(
            &repo,
            "build.yaml",
            "build",
            "@base",
            "out.qcow2",
            r#"files:
  - src: "@://images/botspace/envoy/**/*.yaml"
    dest: /tmp/bake-staging/envoy/
  - src: "@artifact://build/images/payload/*.tar"
    dest: /usr/share/botwork/images/
steps: []
"#,
        );
        let config = load_build_config(repo.path(), &build_path).unwrap();
        assert_eq!(
            config.files,
            vec![
                FileEntry {
                    src: "@://images/botspace/envoy/**/*.yaml".to_string(),
                    dest: "/tmp/bake-staging/envoy/".to_string(),
                    ..Default::default()
                },
                FileEntry {
                    src: "@artifact://build/images/payload/*.tar".to_string(),
                    dest: "/usr/share/botwork/images/".to_string(),
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn test_load_build_config_rejects_top_level_file_bare_path_src() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
files:
  - src: payload/file.txt
    dest: /tmp/payload
steps: []
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("@-reference") || msg.contains("`@`-reference"),
            "error should mention @-reference: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_top_level_file_relative_dest() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
files:
  - src: "@://payload/file.txt"
    dest: relative/path
steps: []
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("absolute"),
            "error should mention absolute dest: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_top_level_file_src_invalid_ref() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
files:
  - src: "@://secret/../etc/passwd"
    dest: /tmp/secret.txt
steps: []
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("..") || msg.contains("invalid"),
            "error should mention traversal or invalid ref: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_top_level_file_glob_with_non_directory_dest() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
files:
  - src: "@artifact://payload/*.tar"
    dest: /tmp/payload.tar
steps: []
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ending with '/'"),
            "error should mention directory dest: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_top_level_file_unknown_field() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
files:
  - src: "@://payload/file.txt"
    dest: /tmp/file.txt
    bogus: 1
steps: []
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("bogus") || msg.contains("unknown field"));
    }

    #[test]
    fn test_load_build_config_parses_top_level_file_permission_fields() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
files:
  - src: "@payload"
    dest: /usr/local/bin/file
    mode: "0755"
    owner: root
    group: root
    overwrite: true
    parents: true
steps: []
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(config.files.len(), 1);
        let file = &config.files[0];
        assert_eq!(file.mode.as_deref(), Some("0755"));
        assert_eq!(file.owner.as_deref(), Some("root"));
        assert_eq!(file.group.as_deref(), Some("root"));
        assert_eq!(file.overwrite, Some(true));
        assert_eq!(file.parents, Some(true));
    }

    #[test]
    fn test_load_build_config_rejects_top_level_file_invalid_mode() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
files:
  - src: "@payload"
    dest: /tmp/file.txt
    mode: "abc"
steps: []
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("mode") && msg.contains("octal"),
            "error should mention mode and octal: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_top_level_file_owner_with_slash() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
files:
  - src: "@payload"
    dest: /tmp/file.txt
    owner: "root/admin"
steps: []
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("owner") && msg.contains('/'),
            "error should mention owner and invalid char: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_top_level_file_group_with_metachar() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
files:
  - src: "@payload"
    dest: /tmp/file.txt
    group: "adm;in"
steps: []
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("group"), "error should mention group: {msg}");
    }

    #[test]
    fn test_load_test_config_files_absent_is_empty() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nsteps: []\n",
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert!(config.files.is_empty(), "files should default to empty");
    }

    #[test]
    fn test_load_test_config_parses_top_level_files() {
        let repo = TempDir::new().unwrap();
        let test_path = test_doc(
            &repo,
            "test.yaml",
            "test",
            r#"files:
  - src: "@://fixtures/envoy/**/*.yaml"
    dest: /tmp/envoy/
steps: []
"#,
        );
        let config = load_test_config(repo.path(), &test_path).unwrap();
        assert_eq!(
            config.files,
            vec![FileEntry {
                src: "@://fixtures/envoy/**/*.yaml".to_string(),
                dest: "/tmp/envoy/".to_string(),
                ..Default::default()
            }]
        );
    }

    #[test]
    fn test_load_test_config_defaults_timeouts() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nsteps: []\n",
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(config.step_timeout, 300);
        assert_eq!(config.timeout, 1800);
        assert_eq!(config.cloud_init_timeout, 300);
    }

    #[test]
    fn test_load_test_config_overrides_timeouts() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nstep_timeout: 600\ntimeout: 2400\nsteps: []\n",
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(config.step_timeout, 600);
        assert_eq!(config.timeout, 2400);
        assert_eq!(config.cloud_init_timeout, 300);
    }

    #[test]
    fn test_load_build_config_rejects_wrong_type() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: botforge/test\nname: test\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        assert!(
            format!("{err:#}").contains("type: botforge/test"),
            "error should mention the actual type: {err:#}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_fragment_type() {
        let repo = TempDir::new().unwrap();
        write_build_config(&repo, "build.yaml", "type: botforge/fragment\nsteps: []\n");
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("type: botforge/fragment"));
    }

    #[test]
    fn test_load_build_config_rejects_legacy_bare_type_as_unknown() {
        let repo = TempDir::new().unwrap();
        let bare_type = "build";
        write_build_config(
            &repo,
            "build.yaml",
            &format!("type: {bare_type}\nname: build\nsteps: []\n"),
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown variant")
                || msg.contains("unknown")
                || msg.contains("did not match any variant"),
            "legacy bare type should be rejected as unknown type: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_rejects_legacy_bare_type_as_unknown() {
        let repo = TempDir::new().unwrap();
        let bare_type = "test";
        write_test_config(
            &repo,
            "test.yaml",
            &format!("type: {bare_type}\nname: test\nsteps: []\n"),
        );
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown variant")
                || msg.contains("unknown")
                || msg.contains("did not match any variant"),
            "legacy bare type should be rejected as unknown type: {msg}"
        );
    }

    #[test]
    fn test_fragment_include_rejects_legacy_bare_type_as_unknown() {
        let repo = TempDir::new().unwrap();
        let bare_type = "fragment";
        write_test_config(
            &repo,
            "frag.yaml",
            &format!("type: {bare_type}\nsteps: []\n"),
        );
        write_test_config(
            &repo,
            "test.yaml",
            "type: botforge/test\nname: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        );
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not a consumable fragment"),
            "legacy bare fragment type should be rejected: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_sets_document_name() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: botforge/build\nname: foo\nimage: \"@base\"\noutput: out.qcow2\nsteps: []\n",
        );
        let cfg = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(cfg.name, "foo");
    }

    #[test]
    fn test_load_test_config_sets_document_name() {
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "test.yaml",
            "type: botforge/test\nname: foo\nsteps: []\n",
        );
        let cfg = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(cfg.name, "foo");
    }

    #[test]
    fn test_load_build_config_requires_name() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: botforge/build\nimage: \"@base\"\noutput: out.qcow2\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        assert!(
            format!("{err:#}").contains("'name' is required in a 'type: botforge/build' document"),
            "missing name should produce required-field error: {err:#}"
        );
    }

    #[test]
    fn test_load_test_config_requires_name() {
        let repo = TempDir::new().unwrap();
        write_test_config(&repo, "test.yaml", "type: botforge/test\nsteps: []\n");
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(
            format!("{err:#}").contains("'name' is required in a 'type: botforge/test' document"),
            "missing name should produce required-field error: {err:#}"
        );
    }

    #[test]
    fn test_load_build_config_accepts_name_separators() {
        let repo = TempDir::new().unwrap();
        for (idx, name) in ["foo/bar", "foo.bar", "foo>bar"].iter().enumerate() {
            write_build_config(
                &repo,
                &format!("build-{idx}.yaml"),
                &format!(
                    "type: botforge/build\nname: {name}\nimage: \"@base\"\noutput: out-{idx}.qcow2\nsteps: []\n"
                ),
            );
            let cfg =
                load_build_config(repo.path(), &repo.path().join(format!("build-{idx}.yaml")))
                    .unwrap();
            assert_eq!(cfg.name, *name);
        }
    }

    #[test]
    fn test_load_test_config_rejects_non_ascii_name() {
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "test.yaml",
            "type: botforge/test\nname: café\nsteps: []\n",
        );
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(
            format!("{err:#}").contains("'name' in a 'type: botforge/test' document")
                && format!("{err:#}").contains("printable ASCII"),
            "non-ASCII name should be rejected: {err:#}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_blank_name() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: botforge/build\nname: \"   \"\nimage: \"@base\"\noutput: out.qcow2\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        assert!(
            format!("{err:#}").contains("'name' is required in a 'type: botforge/build' document"),
            "blank name should be rejected as required-field error: {err:#}"
        );
    }

    #[test]
    fn test_fragment_rejects_top_level_name() {
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "frag.yaml",
            "type: botforge/fragment\nname: nope\nsteps: []\n",
        );
        write_test_config(
            &repo,
            "test.yaml",
            "type: botforge/test\nname: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        );
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("name: is not valid in a 'type: botforge/fragment' document"),
            "fragment should reject top-level name section: {msg}"
        );
    }

    #[test]
    fn test_fragment_step_name_is_still_allowed() {
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "frag.yaml",
            "type: botforge/fragment\nsteps:\n  - on: guest\n    name: still-ok\n    run: echo hi\n",
        );
        write_test_config(
            &repo,
            "test.yaml",
            "type: botforge/test\nname: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        );
        let cfg = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(cfg.steps.len(), 1);
        assert_eq!(run_ref(&cfg.steps[0]).name, "still-ok");
    }

    #[test]
    fn test_load_build_config_rejects_ports_section() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: botforge/build\nname: build\nports:\n  - 80\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ports") && msg.contains("type: botforge/build"),
            "error should mention the offending key and document type: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_isos_section() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: botforge/build\nname: build\nisos:\n  - some.iso\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("isos"));
    }

    #[test]
    fn test_load_build_config_rejects_diagnostics_units_section() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: botforge/build\nname: build\ndiagnostics_units:\n  - foo\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("diagnostics_units"));
    }

    #[test]
    fn test_load_build_config_requires_image() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: botforge/build\nname: build\noutput: \"out.qcow2\"\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("'image'") && msg.contains("required"),
            "error should mention missing image: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_requires_output() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: botforge/build\nname: build\nimage: \"@base\"\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("'output'") && msg.contains("required"),
            "error should mention missing output: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_non_filename_output() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"foo/bar.qcow2\"\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bare filename"),
            "error should mention bare filename requirement: {msg}"
        );

        write_build_config(
            &repo,
            "build-dotdot.yaml",
            "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"../bar.qcow2\"\nsteps: []\n",
        );
        let err =
            load_build_config(repo.path(), &repo.path().join("build-dotdot.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bare filename"),
            "error should mention bare filename requirement for dotdot: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_empty_image() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: botforge/build\nname: build\nimage: \"\"\noutput: \"out.qcow2\"\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("'image'") && msg.contains("required"),
            "error should mention empty image: {msg}"
        );
    }

    #[test]
    fn test_fragment_rejects_image() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            "type: botforge/fragment\nimage: \"@debian-base\"\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("image"), "error should mention image: {msg}");
    }

    #[test]
    fn test_load_test_config_accepts_image_section() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nimage: \"@artifact://foo.qcow2\"\nsteps: []\n",
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(
            config.image,
            Some(Reference::Artifact {
                path: Some(PathBuf::from("foo.qcow2"))
            })
        );
    }

    #[test]
    fn test_load_test_config_rejects_output_section() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\noutput: \"out.qcow2\"\nsteps: []\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("output"), "error should mention output: {msg}");
    }

    #[test]
    fn test_fragment_rejects_output() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            "type: botforge/fragment\noutput: \"out.qcow2\"\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("output"), "error should mention output: {msg}");
    }

    #[test]
    fn test_load_test_config_rejects_disk_size_section() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\ndisk_size: \"20G\"\nsteps: []\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("disk_size") && msg.contains("type: botforge/test"),
            "error should mention the offending key and document type: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_rejects_memsize_section() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nmemsize: 8192\nsteps: []\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("memsize"));
    }

    #[test]
    fn test_load_test_config_rejects_smp_section() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nsmp: 8\nsteps: []\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("smp"));
    }

    #[test]
    fn test_fragment_rejects_disk_size() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            "type: botforge/fragment\ndisk_size: \"20G\"\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("disk_size"));
    }

    #[test]
    fn test_fragment_rejects_memsize() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            "type: botforge/fragment\nmemsize: 8192\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("memsize"));
    }

    #[test]
    fn test_fragment_rejects_smp() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            "type: botforge/fragment\nsmp: 8\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("smp"));
    }

    #[test]
    fn test_fragment_rejects_step_timeout() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            "type: botforge/fragment\nstep_timeout: 600\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("step_timeout"));
    }

    #[test]
    fn test_fragment_rejects_timeout() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            "type: botforge/fragment\ntimeout: 600\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("timeout"));
    }

    #[test]
    fn test_build_config_accepts_fragment_via_uses() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/fragment
steps:
  - on: guest
    name: frag-step
    timeout: 42
    run: echo from-fragment
"#,
        )
        .unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@debian-base"
output: "out.qcow2"
steps:
  - uses: "@://frag.yaml"
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).name, "frag-step");
        assert_eq!(run_ref(&config.steps[0]).timeout, Some(42));
    }

    #[test]
    fn test_build_config_expands_step_level_for() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@debian-base"
output: "out.qcow2"
steps:
  - name: "build-${{ args.0 }}"
    for: [alpha, beta]
    run: echo ${{ args.0 }}
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(config.steps.len(), 2);
        assert_eq!(run_ref(&config.steps[0]).name, "build-alpha");
        assert_eq!(run_ref(&config.steps[0]).run, "echo alpha");
        assert_eq!(run_ref(&config.steps[1]).name, "build-beta");
        assert_eq!(run_ref(&config.steps[1]).run, "echo beta");
    }

    #[test]
    fn test_build_config_expands_fragment_scalar_for_via_uses() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/fragment
steps:
  - name: "build-${{ args.0 }}"
    for: [alpha, beta]
    run: echo ${{ args.0 }}
"#,
        )
        .unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@debian-base"
output: "out.qcow2"
steps:
  - uses: "@://frag.yaml"
"#,
        );

        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();

        assert_eq!(config.steps.len(), 2);
        assert_eq!(run_ref(&config.steps[0]).name, "build-alpha");
        assert_eq!(run_ref(&config.steps[0]).run, "echo alpha");
        assert_eq!(run_ref(&config.steps[1]).name, "build-beta");
        assert_eq!(run_ref(&config.steps[1]).run, "echo beta");
    }

    #[test]
    fn test_build_config_preserves_fragment_sudo_via_uses() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/fragment
steps:
  - on: guest
    name: frag-step
    sudo: true
    run: echo from-fragment
"#,
        )
        .unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@debian-base"
output: "out.qcow2"
steps:
  - uses: "@://frag.yaml"
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).name, "frag-step");
        assert_eq!(run_ref(&config.steps[0]).sudo, Some(true));
    }

    #[test]
    fn test_load_build_config_accepts_expect_block() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@debian-base"
output: "out.qcow2"
steps:
  - name: expect-step
    run: echo ok
    expect:
      exit: 0
      stdout:
        contains: ["ok"]
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).name, "expect-step");
        assert_eq!(
            run_ref(&config.steps[0]).expect,
            Some(ExpectBlock {
                exit: Some(0),
                stdout: Some(StdioExpect {
                    contains: vec!["ok".to_string()],
                    not_contains: vec![],
                }),
                stderr: None,
            })
        );
    }

    #[test]
    fn test_build_config_preserves_fragment_expect_via_uses() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/fragment
steps:
  - on: guest
    name: frag-step
    run: echo from-fragment
    expect:
      exit: 0
      stdout:
        contains: ["from-fragment"]
"#,
        )
        .unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@debian-base"
output: "out.qcow2"
steps:
  - uses: "@://frag.yaml"
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).name, "frag-step");
        assert_eq!(
            run_ref(&config.steps[0]).expect,
            Some(ExpectBlock {
                exit: Some(0),
                stdout: Some(StdioExpect {
                    contains: vec!["from-fragment".to_string()],
                    not_contains: vec![],
                }),
                stderr: None,
            })
        );
    }

    #[test]
    fn test_build_config_fragment_input_substitution_preserves_step_timeout() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: botforge/fragment
inputs:
  seconds:
    type: number
    required: true
steps:
  - on: guest
    name: frag-step
    timeout: ${{ inputs.seconds }}
    run: echo from-fragment
"#,
        )
        .unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@debian-base"
output: "out.qcow2"
steps:
  - uses: "@://frag.yaml"
    with:
      seconds: "75"
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(run_ref(&config.steps[0]).timeout, Some(75));
    }

    #[test]
    fn test_load_test_config_rejects_non_positive_document_timeouts() {
        let repo = TempDir::new().unwrap();
        for (name, content, needle) in [
            (
                "test-zero-step-timeout.yaml",
                "type: botforge/test\nname: test\nstep_timeout: 0\nsteps: []\n",
                "positive integer",
            ),
            (
                "test-negative-timeout.yaml",
                "type: botforge/test\nname: test\ntimeout: -1\nsteps: []\n",
                "positive integer",
            ),
        ] {
            std::fs::write(repo.path().join(name), content).unwrap();
            let err = load_test_config(repo.path(), &repo.path().join(name)).unwrap_err();
            assert!(
                format!("{err:#}").contains(needle),
                "error should mention invalid timeout value: {err:#}"
            );
        }
    }

    #[test]
    fn test_load_build_config_rejects_non_positive_timeouts() {
        let repo = TempDir::new().unwrap();
        for (name, content, needle) in [
            (
                "build-zero-step-timeout.yaml",
                "type: botforge/build\nname: build\nimage: \"@debian-base\"\noutput: \"out.qcow2\"\nstep_timeout: 0\nsteps: []\n",
                "positive integer",
            ),
            (
                "build-negative-step-timeout.yaml",
                "type: botforge/build\nname: build\nimage: \"@debian-base\"\noutput: \"out.qcow2\"\nsteps:\n  - on: host\n    name: slow\n    timeout: -5\n    run: echo ok\n",
                "positive integer",
            ),
        ] {
            write_build_config(&repo, name, content);
            let err = load_build_config(repo.path(), &repo.path().join(name)).unwrap_err();
            assert!(
                format!("{err:#}").contains(needle),
                "error should mention invalid timeout value: {err:#}"
            );
        }
    }

    #[test]
    fn test_build_config_cannot_be_used_as_fragment() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build-base.yaml",
            "type: botforge/build\nname: build\nsteps:\n  - on: guest\n    name: s\n    run: echo ok\n",
        );
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nsteps:\n  - uses: \"@://build-base.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not a consumable fragment"),
            "error should reject build doc as fragment: {msg}"
        );
    }
}

mod archive {
    use super::*;

    // --- validate_build_steps ---

    #[test]
    fn test_validate_build_steps_accepts_guest_step() {
        let steps = vec![make_step(StepTarget::Guest, "s")];
        assert!(validate_build_steps(&steps).is_ok());
    }

    #[test]
    fn test_validate_build_steps_accepts_host_step_without_ports() {
        // Unlike test, build does not require ports for host steps.
        let steps = vec![make_step(StepTarget::Host, "h")];
        assert!(validate_build_steps(&steps).is_ok());
    }

    #[test]
    fn test_validate_build_steps_rejects_host_step_with_sudo() {
        let mut step = make_step(StepTarget::Host, "host-root");
        let TestStep::Run(run) = &mut step else {
            panic!("expected run step");
        };
        run.sudo = Some(true);
        let err = validate_build_steps(&[step]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("host-root"),
            "error should mention step name: {msg}"
        );
        assert!(msg.contains("sudo"), "error should mention sudo: {msg}");
        assert!(msg.contains("guest"), "error should mention guest: {msg}");
    }

    #[test]
    fn test_validate_build_steps_accepts_host_step_with_explicit_sudo_false() {
        let mut step = make_step(StepTarget::Host, "host-unprivileged");
        let TestStep::Run(run) = &mut step else {
            panic!("expected run step");
        };
        run.sudo = Some(false);
        assert!(validate_build_steps(&[step]).is_ok());
    }

    #[test]
    fn test_validate_build_steps_accepts_guest_step_with_sudo() {
        let mut step = make_step(StepTarget::Guest, "guest-root");
        let TestStep::Run(run) = &mut step else {
            panic!("expected run step");
        };
        run.sudo = Some(true);
        assert!(validate_build_steps(&[step]).is_ok());
    }

    #[test]
    fn test_validate_build_steps_rejects_bad_shell() {
        let mut step = make_step(StepTarget::Guest, "bad-shell");
        let TestStep::Run(run) = &mut step else {
            panic!("expected run step");
        };
        run.shell = Some("fish".to_string());
        let err = validate_build_steps(&[step]).unwrap_err();
        assert!(format!("{err:#}").contains("fish"));
    }

    #[test]
    fn test_validate_build_steps_accepts_expect_block() {
        let mut step = make_step(StepTarget::Guest, "assert-step");
        let TestStep::Run(run) = &mut step else {
            panic!("expected run step");
        };
        run.expect = Some(ExpectBlock {
            exit: Some(0),
            stdout: Some(StdioExpect {
                contains: vec!["ok".to_string()],
                not_contains: vec![],
            }),
            stderr: None,
        });
        assert!(validate_build_steps(&[step]).is_ok());
    }

    #[test]
    fn test_validate_build_steps_accepts_step_without_expect() {
        let step = make_step(StepTarget::Guest, "no-expect");
        assert!(validate_build_steps(&[step]).is_ok());
    }

    #[test]
    fn test_build_step_deserialize_archive_shape() {
        let step: TestStep = serde_yaml::from_str(
            r#"
archive:
  src: "@some-tool"
  into: some-tool
  name: unpack-some-tool
"#,
        )
        .unwrap();
        let TestStep::Archive(archive) = step else {
            panic!("expected archive step");
        };
        assert_eq!(archive.archive.src, "@some-tool");
        assert_eq!(archive.archive.into.as_deref(), Some("some-tool"));
        assert_eq!(archive.archive.name.as_deref(), Some("unpack-some-tool"));
    }

    #[test]
    fn test_build_step_deserialize_run_shape_still_works() {
        let step: TestStep = serde_yaml::from_str(
            r#"
on: guest
name: run-it
run: echo ok
"#,
        )
        .unwrap();
        let TestStep::Run(step) = step else {
            panic!("expected run step");
        };
        assert_eq!(step.name, "run-it");
        assert_eq!(step.run, "echo ok");
    }

    #[test]
    fn test_validate_build_steps_accepts_archive_step() {
        let steps = vec![TestStep::Archive(ArchiveStep {
            archive: ArchiveStepSpec {
                src: "@some-tool".to_string(),
                into: Some("some-tool".to_string()),
                name: Some("unpack".to_string()),
                dest: None,
            },
            target: None,
            run: None,
            timeout: None,
            shell: None,
        })];
        assert!(validate_build_steps(&steps).is_ok());
    }

    #[test]
    fn test_validate_build_steps_rejects_archive_empty_src() {
        let steps = vec![TestStep::Archive(ArchiveStep {
            archive: ArchiveStepSpec {
                src: "   ".to_string(),
                into: None,
                name: Some("bad-archive".to_string()),
                dest: None,
            },
            target: None,
            run: None,
            timeout: None,
            shell: None,
        })];
        let err = validate_build_steps(&steps).unwrap_err();
        assert!(format!("{err:#}").contains("src"));
        assert!(format!("{err:#}").contains("bad-archive"));
    }

    #[test]
    fn test_validate_build_steps_rejects_archive_without_at_prefix() {
        let steps = vec![TestStep::Archive(ArchiveStep {
            archive: ArchiveStepSpec {
                src: "some-tool".to_string(),
                into: None,
                name: Some("bad-archive".to_string()),
                dest: None,
            },
            target: None,
            run: None,
            timeout: None,
            shell: None,
        })];
        let err = validate_build_steps(&steps).unwrap_err();
        assert!(format!("{err:#}").contains("must start with '@'"));
    }

    #[test]
    fn test_validate_build_steps_rejects_archive_with_forbidden_fields() {
        let steps = vec![TestStep::Archive(ArchiveStep {
            archive: ArchiveStepSpec {
                src: "@some-tool".to_string(),
                into: None,
                name: Some("bad-archive".to_string()),
                dest: None,
            },
            target: Some(StepTarget::Host),
            run: Some("echo hi".to_string()),
            timeout: Some(30),
            shell: Some("bash".to_string()),
        })];
        let err = validate_build_steps(&steps).unwrap_err();
        // run/shell/timeout are still forbidden regardless of on: host.
        assert!(
            format!("{err:#}").contains("run")
                || format!("{err:#}").contains("shell")
                || format!("{err:#}").contains("timeout"),
            "error should mention a forbidden field: {err:#}"
        );
    }

    #[test]
    fn test_validate_build_steps_rejects_archive_src_traversal_scheme() {
        let steps = vec![TestStep::Archive(ArchiveStep {
            archive: ArchiveStepSpec {
                src: "@://provider/asset".to_string(),
                into: None,
                name: Some("bad-archive".to_string()),
                dest: None,
            },
            target: None,
            run: None,
            timeout: None,
            shell: None,
        })];
        let err = validate_build_steps(&steps).unwrap_err();
        assert!(format!("{err:#}").contains("@://"));
    }

    #[test]
    fn test_validate_build_steps_accepts_explicit_on_host_archive_step() {
        // on: host is now a legal explicit spelling of the default.
        let steps = vec![TestStep::Archive(ArchiveStep {
            archive: ArchiveStepSpec {
                src: "@some-tool".to_string(),
                into: None,
                name: Some("fetch-tool".to_string()),
                dest: None,
            },
            target: Some(StepTarget::Host),
            run: None,
            timeout: None,
            shell: None,
        })];
        assert!(validate_build_steps(&steps).is_ok());
    }

    #[test]
    fn test_validate_build_steps_accepts_guest_archive_with_absolute_dest() {
        let steps = vec![TestStep::Archive(ArchiveStep {
            archive: ArchiveStepSpec {
                src: "@some-tool".to_string(),
                into: None,
                name: Some("install-tool".to_string()),
                dest: Some("/var/lib/foo".to_string()),
            },
            target: Some(StepTarget::Guest),
            run: None,
            timeout: None,
            shell: None,
        })];
        assert!(validate_build_steps(&steps).is_ok());
    }

    #[test]
    fn test_validate_build_steps_rejects_guest_archive_without_dest() {
        let steps = vec![TestStep::Archive(ArchiveStep {
            archive: ArchiveStepSpec {
                src: "@some-tool".to_string(),
                into: None,
                name: Some("bad-guest".to_string()),
                dest: None,
            },
            target: Some(StepTarget::Guest),
            run: None,
            timeout: None,
            shell: None,
        })];
        let err = validate_build_steps(&steps).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("dest"), "error should mention 'dest': {msg}");
        assert!(
            msg.contains("bad-guest"),
            "error should mention step name: {msg}"
        );
    }

    #[test]
    fn test_validate_build_steps_rejects_guest_archive_with_relative_dest() {
        let steps = vec![TestStep::Archive(ArchiveStep {
            archive: ArchiveStepSpec {
                src: "@some-tool".to_string(),
                into: None,
                name: Some("bad-dest".to_string()),
                dest: Some("relative/path".to_string()),
            },
            target: Some(StepTarget::Guest),
            run: None,
            timeout: None,
            shell: None,
        })];
        let err = validate_build_steps(&steps).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("absolute"),
            "error should mention absolute path: {msg}"
        );
    }

    #[test]
    fn test_validate_build_steps_rejects_host_archive_with_dest() {
        // dest is only valid on on: guest — reject it for on: host or omitted.
        for (label, target) in [("on: host", Some(StepTarget::Host)), ("on: omitted", None)] {
            let steps = vec![TestStep::Archive(ArchiveStep {
                archive: ArchiveStepSpec {
                    src: "@some-tool".to_string(),
                    into: None,
                    name: Some("bad-dest".to_string()),
                    dest: Some("/var/lib/foo".to_string()),
                },
                target,
                run: None,
                timeout: None,
                shell: None,
            })];
            let err = validate_build_steps(&steps).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("dest"),
                "error should mention 'dest' ({label}): {msg}"
            );
        }
    }

    #[test]
    fn test_build_step_deserialize_archive_guest_mode() {
        let step: TestStep = serde_yaml::from_str(
            r#"
on: guest
archive:
  src: "@some-tool"
  name: install-some-tool
  dest: /var/lib/foo
"#,
        )
        .unwrap();
        let TestStep::Archive(archive) = step else {
            panic!("expected archive step");
        };
        assert_eq!(archive.target, Some(StepTarget::Guest));
        assert_eq!(archive.archive.src, "@some-tool");
        assert_eq!(archive.archive.name.as_deref(), Some("install-some-tool"));
        assert_eq!(archive.archive.dest.as_deref(), Some("/var/lib/foo"));
    }

    #[test]
    fn test_build_step_deserialize_archive_host_mode_dest_absent() {
        // Host-mode archive (on: omitted) keeps dest absent.
        let step: TestStep = serde_yaml::from_str(
            r#"
archive:
  src: "@some-tool"
  into: some-tool
  name: unpack-some-tool
"#,
        )
        .unwrap();
        let TestStep::Archive(archive) = step else {
            panic!("expected archive step");
        };
        assert!(archive.target.is_none());
        assert!(archive.archive.dest.is_none());
    }

    #[test]
    fn test_load_build_config_rejects_archive_step_mixed_with_run_fields() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
base-image: @debian-base
steps:
  - archive:
      src: "@some-tool"
      name: bad-mixed
    on: host
    run: echo nope
"#,
        );
        let err = match load_build_config(repo.path(), &repo.path().join("build.yaml")) {
            Err(err) => err,
            Ok(config) => validate_build_steps(&config.steps)
                .expect_err("archive step mixed with run fields must be rejected"),
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("on")
                || msg.contains("run")
                || msg.contains("archive")
                || msg.contains("unknown field"),
            "error should indicate archive/run field conflict: {msg}"
        );
    }
}

// ─── publish config tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod publish_config_tests {
    use super::*;
    use crate::config::load_publish_config;

    fn write_publish_doc(repo: &TempDir, filename: &str, content: &str) -> PathBuf {
        let path = repo.path().join(filename);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn publish_minimal_fs_target_parses() {
        let repo = TempDir::new().unwrap();
        let path = write_publish_doc(
            &repo,
            "release.yaml",
            r#"
type: botforge/publish
name: my-release
fs:
  src: "@artifact://images/vm.qcow2"
  dest: /tmp/releases/
"#,
        );
        let cfg = load_publish_config(&path).unwrap();
        assert!(cfg.fs.is_some(), "fs target should be present");
        assert!(cfg.s3.is_none(), "s3 target should be absent");
        let fs = cfg.fs.unwrap();
        assert_eq!(fs.src, "@artifact://images/vm.qcow2");
        assert_eq!(fs.dest, "/tmp/releases/");
    }

    #[test]
    fn publish_minimal_s3_target_parses() {
        let repo = TempDir::new().unwrap();
        let path = write_publish_doc(
            &repo,
            "release.yaml",
            r#"
type: botforge/publish
name: my-release
s3:
  src: "@artifact://images/vm.qcow2"
  dest: s3://my-bucket/releases/
"#,
        );
        let cfg = load_publish_config(&path).unwrap();
        assert!(cfg.s3.is_some(), "s3 target should be present");
        let s3 = cfg.s3.unwrap();
        assert_eq!(s3.src, "@artifact://images/vm.qcow2");
        assert_eq!(s3.dest, "s3://my-bucket/releases/");
    }

    #[test]
    fn publish_both_targets_parses() {
        let repo = TempDir::new().unwrap();
        let path = write_publish_doc(
            &repo,
            "release.yaml",
            r#"
type: botforge/publish
name: dual-target
fs:
  src: "@artifact://vm.qcow2"
  dest: /tmp/dest/
s3:
  src: "@artifact://vm.qcow2"
  dest: s3://bucket/path/
"#,
        );
        let cfg = load_publish_config(&path).unwrap();
        assert!(cfg.fs.is_some());
        assert!(cfg.s3.is_some());
    }

    #[test]
    fn publish_unknown_target_fails_clearly() {
        let repo = TempDir::new().unwrap();
        let path = write_publish_doc(
            &repo,
            "release.yaml",
            r#"
type: botforge/publish
name: my-release
github:
  version: "@artifact://VERSION"
  message: "@artifact://changelog"
"#,
        );
        let err = load_publish_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("github") || msg.contains("unknown field"),
            "error should mention unknown field 'github': {msg}"
        );
    }

    #[test]
    fn publish_wrong_type_fails_clearly() {
        let repo = TempDir::new().unwrap();
        // Use a document that has no unknown fields but the wrong type.
        let path = write_publish_doc(&repo, "test.yaml", "type: botforge/test\nname: foo\n");
        let err = load_publish_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("botforge/publish") || msg.contains("botforge/test"),
            "error should mention the wrong type: {msg}"
        );
    }

    #[test]
    fn publish_src_must_be_at_reference() {
        let repo = TempDir::new().unwrap();
        let path = write_publish_doc(
            &repo,
            "release.yaml",
            r#"
type: botforge/publish
name: bad-src
fs:
  src: "some/plain/path"
  dest: /tmp/dest/
"#,
        );
        let err = load_publish_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("@-reference") || msg.contains("must be an @"),
            "error should require @-reference: {msg}"
        );
    }

    #[test]
    fn publish_s3_dest_must_start_with_s3_scheme() {
        let repo = TempDir::new().unwrap();
        let path = write_publish_doc(
            &repo,
            "release.yaml",
            r#"
type: botforge/publish
name: bad-dest
s3:
  src: "@artifact://vm.qcow2"
  dest: https://bucket.example.com/path/
"#,
        );
        let err = load_publish_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("s3://") || msg.contains("S3 URL"),
            "error should mention s3:// requirement: {msg}"
        );
    }

    #[test]
    fn publish_missing_name_fails() {
        let repo = TempDir::new().unwrap();
        let path = write_publish_doc(
            &repo,
            "release.yaml",
            r#"
type: botforge/publish
fs:
  src: "@artifact://vm.qcow2"
  dest: /tmp/
"#,
        );
        let err = load_publish_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("name"),
            "error should mention missing 'name': {msg}"
        );
    }

    #[test]
    fn publish_empty_plan_with_name_parses() {
        let repo = TempDir::new().unwrap();
        let path = write_publish_doc(
            &repo,
            "release.yaml",
            "type: botforge/publish\nname: empty-release\n",
        );
        let cfg = load_publish_config(&path).unwrap();
        assert!(cfg.fs.is_none());
        assert!(cfg.s3.is_none());
    }

    #[test]
    fn publish_repo_at_reference_is_valid_src() {
        let repo = TempDir::new().unwrap();
        let path = write_publish_doc(
            &repo,
            "release.yaml",
            r#"
type: botforge/publish
name: repo-src
fs:
  src: "@://path/to/file.txt"
  dest: /tmp/dest/
"#,
        );
        // @:// references (repo-root) should be accepted as valid @-references.
        assert!(load_publish_config(&path).is_ok());
    }
}

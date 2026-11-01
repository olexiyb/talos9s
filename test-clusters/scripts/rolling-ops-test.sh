#!/usr/bin/env bash
#
# Rolling Operations Test Cluster
#
# Creates a 4-node Talos cluster (1 control plane + 3 workers) for testing
# rolling operations in talos9s.
#
# Usage:
#   ./rolling-ops-test.sh create     Create the test cluster
#   ./rolling-ops-test.sh destroy    Destroy the test cluster
#   ./rolling-ops-test.sh status     Show cluster status
#   ./rolling-ops-test.sh help       Show help
#

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Override defaults for rolling operations testing
export TALOS_WORKERS=3
export TALOS_CLUSTER_NAME="${TALOS_CLUSTER_NAME:-talos9s}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[INFO]${NC} $*"; }
log_success() { echo -e "${GREEN}[OK]${NC} $*"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*" >&2; }

OUTPUT_DIR="${SCRIPT_DIR}/../output"

show_help() {
    cat << 'EOF'
Rolling Operations Test Cluster

Creates a 4-node Talos cluster specifically for testing rolling operations:
  - 1 control plane node (10.5.0.2)
  - 3 worker nodes (10.5.0.3, 10.5.0.4, 10.5.0.5)

USAGE:
    ./rolling-ops-test.sh <command>

COMMANDS:
    create      Create the 4-node test cluster with workloads
    destroy     Destroy the test cluster
    status      Show cluster and node status
    help        Show this help

TESTING ROLLING OPERATIONS:

After cluster creation:

1. Start talos9s:
   export KUBECONFIG=test-clusters/output/kubeconfig
   cargo run --bin talos9s

2. Navigate to the cluster view (nodes list)

3. Press 'O' (capital O) to open Rolling Operations

4. Select nodes using arrow keys and Space/Enter:
   - Selected nodes show [1], [2], [3] indicating execution order
   - Workers are safest to test with
   - Control plane can be selected but use caution

5. Choose operation:
   - 'd' = Rolling Drain (cordons, drains pods, then uncordons)
   - 'r' = Rolling Reboot (cordons, drains, reboots, waits for ready, uncordons)

6. Confirm with 'y' to start

NOTES:
- Docker-based reboots are container restarts (~5-10 seconds)
- PDB workloads may cause drain delays (tests PDB handling)
- Audit log written to ~/.talos9s/audit.log

EOF
}

create_cluster() {
    log_info "Creating 4-node cluster for rolling operations testing..."
    log_info "  Control planes: 1"
    log_info "  Workers: 3"
    log_info "  Total nodes: 4"
    echo ""

    # Use the main cluster script with workers override
    "${SCRIPT_DIR}/cluster.sh" create flannel "$@"

    echo ""
    log_info "Setting up test workloads for rolling operations..."
    echo ""

    # Wait a moment for cluster to stabilize
    sleep 5

    # Create drainable workloads
    export KUBECONFIG="${OUTPUT_DIR}/kubeconfig"
    "${SCRIPT_DIR}/cluster.sh" workloads drainable

    echo ""
    log_success "Rolling operations test cluster ready!"
    echo ""
    echo -e "${CYAN}=== Node Layout ===${NC}"
    echo ""
    kubectl get nodes -o wide 2>/dev/null || true
    echo ""
    echo -e "${CYAN}=== Test Workloads ===${NC}"
    echo ""
    kubectl get pods -n test-drainable -o wide 2>/dev/null || true
    echo ""
    echo -e "${CYAN}=== Next Steps ===${NC}"
    echo ""
    echo "1. Export kubeconfig:"
    echo "   export KUBECONFIG=${OUTPUT_DIR}/kubeconfig"
    echo ""
    echo "2. Start talos9s:"
    echo "   cargo run --bin talos9s"
    echo ""
    echo "3. Press 'O' (capital O) in cluster view for Rolling Operations"
    echo ""
    echo "4. Select worker nodes [1], [2], [3] and choose operation:"
    echo "   - 'd' for rolling drain"
    echo "   - 'r' for rolling reboot"
    echo ""
}

destroy_cluster() {
    log_info "Destroying rolling operations test cluster..."
    "${SCRIPT_DIR}/cluster.sh" destroy
}

show_status() {
    echo ""
    echo -e "${CYAN}=== Rolling Operations Test Cluster Status ===${NC}"
    echo ""

    export KUBECONFIG="${OUTPUT_DIR}/kubeconfig"

    echo -e "${BLUE}--- Nodes ---${NC}"
    kubectl get nodes -o wide 2>/dev/null || log_warn "Could not get nodes"
    echo ""

    echo -e "${BLUE}--- Node Conditions ---${NC}"
    kubectl get nodes -o custom-columns=\
'NAME:.metadata.name,'\
'STATUS:.status.conditions[?(@.type=="Ready")].status,'\
'SCHEDULABLE:.spec.unschedulable' 2>/dev/null || true
    echo ""

    echo -e "${BLUE}--- Test Workloads ---${NC}"
    kubectl get pods -n test-drainable -o wide 2>/dev/null || log_warn "No test workloads found"
    echo ""

    echo -e "${BLUE}--- Pod Distribution ---${NC}"
    kubectl get pods -n test-drainable -o custom-columns=\
'NAME:.metadata.name,'\
'NODE:.spec.nodeName,'\
'STATUS:.status.phase' 2>/dev/null || true
    echo ""

    # Check for cordoned nodes
    local cordoned
    cordoned=$(kubectl get nodes -o json 2>/dev/null | jq -r '.items[] | select(.spec.unschedulable==true) | .metadata.name' || true)
    if [[ -n "${cordoned}" ]]; then
        echo -e "${YELLOW}--- Cordoned Nodes ---${NC}"
        echo "${cordoned}"
        echo ""
        echo "To uncordon: kubectl uncordon <node-name>"
        echo ""
    fi

    # Show audit log tail if it exists
    local audit_log="${HOME}/.talos9s/audit.log"
    if [[ -f "${audit_log}" ]]; then
        echo -e "${BLUE}--- Recent Audit Log ---${NC}"
        tail -10 "${audit_log}" 2>/dev/null || true
        echo ""
    fi
}

main() {
    local command="${1:-help}"
    shift || true

    case "${command}" in
        create)
            create_cluster "$@"
            ;;
        destroy)
            destroy_cluster
            ;;
        status)
            show_status
            ;;
        help|--help|-h)
            show_help
            ;;
        *)
            log_error "Unknown command: ${command}"
            echo "Run './rolling-ops-test.sh help' for usage"
            exit 1
            ;;
    esac
}

main "$@"

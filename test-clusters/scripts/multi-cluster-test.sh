#!/usr/bin/env bash
#
# Multi-Cluster Test Script
#
# Creates 3 Talos clusters for testing the multi-cluster accordion UI in talos9s.
# Each cluster has 1 control plane + 3 workers (4 nodes total).
#
# Usage:
#   ./multi-cluster-test.sh create     Create all 3 test clusters
#   ./multi-cluster-test.sh destroy    Destroy all test clusters
#   ./multi-cluster-test.sh status     Show status of all clusters
#   ./multi-cluster-test.sh help       Show help
#

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUTPUT_DIR="${SCRIPT_DIR}/../output"

# Cluster configurations
# Format: name:cidr:api_port
CLUSTERS=(
    "cluster-alpha:10.5.0.0/24:6443"
    "cluster-beta:10.6.0.0/24:6444"
    "cluster-gamma:10.7.0.0/24:6445"
)

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
MAGENTA='\033[0;35m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[INFO]${NC} $*"; }
log_success() { echo -e "${GREEN}[OK]${NC} $*"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*" >&2; }
log_cluster() { echo -e "${MAGENTA}[CLUSTER]${NC} $*"; }

mkdir -p "${OUTPUT_DIR}"

show_help() {
    cat << 'EOF'
Multi-Cluster Test Script

Creates 3 Talos clusters for testing the multi-cluster accordion UI:
  - cluster-alpha (10.5.0.0/24, port 6443)
  - cluster-beta  (10.6.0.0/24, port 6444)
  - cluster-gamma (10.7.0.0/24, port 6445)

Each cluster has 1 control plane + 2 workers.

USAGE:
    ./multi-cluster-test.sh <command>

COMMANDS:
    create      Create all 3 test clusters (sequentially)
    destroy     Destroy all test clusters
    status      Show status of all clusters
    help        Show this help

TESTING MULTI-CLUSTER UI:

After cluster creation:

1. Your talosconfig will have all 3 cluster contexts
2. Start talos9s:
   cargo run --bin talos9s

3. You should see the accordion with all 3 clusters:
   ▸ ▼ ● cluster-alpha (3)
       ▼ Control Plane (1)
          ● cluster-alpha-controlplane-1
       ▼ Workers (2)
          ● cluster-alpha-worker-1
          ● cluster-alpha-worker-2
     ▶ ● cluster-beta (3)
     ▶ ● cluster-gamma (3)

4. Use Space/Enter to expand/collapse clusters
5. Press 'O' on any cluster to test rolling operations

NOTES:
- Creating 3 clusters takes several minutes
- Each cluster uses ~8GB RAM (total ~24GB recommended)
- Docker must be running with sufficient resources

EOF
}

check_prerequisites() {
    local missing=()

    command -v talosctl &>/dev/null || missing+=("talosctl")
    command -v kubectl &>/dev/null || missing+=("kubectl")
    command -v docker &>/dev/null || missing+=("docker")

    if [[ ${#missing[@]} -gt 0 ]]; then
        log_error "Missing required tools: ${missing[*]}"
        exit 1
    fi

    if ! docker info &>/dev/null; then
        log_error "Docker is not running"
        exit 1
    fi
}

# Parse cluster config string
parse_cluster_config() {
    local config="$1"
    IFS=':' read -r CLUSTER_NAME CLUSTER_CIDR CLUSTER_PORT <<< "$config"
}

# Get control plane IP from CIDR (assumes .2 for control plane)
get_cp_ip() {
    local cidr="$1"
    echo "${cidr%.*}.2"
}

# Wait for Talos API to be ready
wait_for_talos_api() {
    local cp_ip="$1"
    local timeout="${2:-120}"
    local start_time=$(date +%s)

    log_info "Waiting for Talos API at ${cp_ip}..."
    while true; do
        if talosctl version --nodes "${cp_ip}" &>/dev/null; then
            log_success "Talos API is ready"
            return 0
        fi

        local elapsed=$(($(date +%s) - start_time))
        if [[ $elapsed -ge $timeout ]]; then
            log_error "Timeout waiting for Talos API"
            return 1
        fi
        sleep 2
    done
}

# Wait for Kubernetes API to be ready
wait_for_k8s_api() {
    local kubeconfig="$1"
    local timeout="${2:-180}"
    local start_time=$(date +%s)

    log_info "Waiting for Kubernetes API..."
    while true; do
        if KUBECONFIG="${kubeconfig}" kubectl get nodes &>/dev/null; then
            log_success "Kubernetes API is ready"
            return 0
        fi

        local elapsed=$(($(date +%s) - start_time))
        if [[ $elapsed -ge $timeout ]]; then
            log_error "Timeout waiting for Kubernetes API"
            return 1
        fi
        printf "\r  Waiting for K8s API... (%ds elapsed)" "$elapsed"
        sleep 3
    done
}

# Wait for all nodes to be Ready
wait_for_nodes_ready() {
    local kubeconfig="$1"
    local expected_nodes="$2"
    local timeout="${3:-300}"
    local start_time=$(date +%s)

    log_info "Waiting for ${expected_nodes} nodes to be Ready..."
    while true; do
        local ready_nodes
        ready_nodes=$(KUBECONFIG="${kubeconfig}" kubectl get nodes --no-headers 2>/dev/null | grep -c " Ready " || echo "0")

        if [[ "$ready_nodes" -ge "$expected_nodes" ]]; then
            log_success "All ${expected_nodes} nodes are Ready"
            return 0
        fi

        local elapsed=$(($(date +%s) - start_time))
        if [[ $elapsed -ge $timeout ]]; then
            log_error "Timeout waiting for nodes (${ready_nodes}/${expected_nodes} ready)"
            return 1
        fi
        printf "\r  Waiting for nodes... (%d/%d ready, %ds elapsed)" "$ready_nodes" "$expected_nodes" "$elapsed"
        sleep 5
    done
}

# Create drainable workloads for a cluster
create_workloads() {
    local kubeconfig="$1"
    local cluster_name="$2"

    log_info "Creating test workloads for ${cluster_name}..."

    # Create namespace
    KUBECONFIG="${kubeconfig}" kubectl create namespace test-drainable 2>/dev/null || true

    # Uncordon all nodes and remove control plane taint
    KUBECONFIG="${kubeconfig}" kubectl uncordon --all 2>/dev/null || true
    KUBECONFIG="${kubeconfig}" kubectl taint nodes --all node-role.kubernetes.io/control-plane:NoSchedule- 2>/dev/null || true

    # Create web deployment
    KUBECONFIG="${kubeconfig}" kubectl apply -f - <<EOF
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
  namespace: test-drainable
spec:
  replicas: 3
  selector:
    matchLabels:
      app: web
  template:
    metadata:
      labels:
        app: web
    spec:
      containers:
      - name: nginx
        image: nginx:alpine
        ports:
        - containerPort: 80
      terminationGracePeriodSeconds: 10
EOF

    # Create api deployment
    KUBECONFIG="${kubeconfig}" kubectl apply -f - <<EOF
apiVersion: apps/v1
kind: Deployment
metadata:
  name: api
  namespace: test-drainable
spec:
  replicas: 2
  selector:
    matchLabels:
      app: api
  template:
    metadata:
      labels:
        app: api
    spec:
      containers:
      - name: nginx
        image: nginx:alpine
        ports:
        - containerPort: 80
      terminationGracePeriodSeconds: 10
EOF

    # Create PDB for web
    KUBECONFIG="${kubeconfig}" kubectl apply -f - <<EOF
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: web-pdb
  namespace: test-drainable
spec:
  maxUnavailable: 1
  selector:
    matchLabels:
      app: web
EOF

    log_success "Created workloads for ${cluster_name}"
}

# Create a single cluster
create_cluster() {
    local config="$1"
    parse_cluster_config "$config"

    local cp_ip
    cp_ip=$(get_cp_ip "$CLUSTER_CIDR")

    echo ""
    log_cluster "Creating ${CLUSTER_NAME}..."
    log_info "  Network: ${CLUSTER_CIDR}"
    log_info "  Control plane: ${cp_ip}"
    log_info "  Workers: 2"
    echo ""

    # Check if cluster already exists
    if docker ps -a --format '{{.Names}}' | grep -q "^${CLUSTER_NAME}-"; then
        log_warn "Cluster ${CLUSTER_NAME} already exists, destroying first..."
        talosctl cluster destroy --name "${CLUSTER_NAME}" 2>/dev/null || true
        sleep 2
    fi

    # Create the cluster with Flannel CNI using Docker provisioner
    talosctl cluster create docker \
        --name "${CLUSTER_NAME}" \
        --workers 2 \
        --subnet "${CLUSTER_CIDR}" &
    local cluster_pid=$!

    # Wait for Talos API (while cluster create runs in background)
    wait_for_talos_api "${cp_ip}" 120

    # Export kubeconfig
    local kubeconfig="${OUTPUT_DIR}/${CLUSTER_NAME}-kubeconfig"
    log_info "Exporting kubeconfig to ${kubeconfig}..."
    talosctl kubeconfig "${kubeconfig}" --nodes "${cp_ip}" --force
    log_success "Kubeconfig exported"

    # Wait for Kubernetes API
    wait_for_k8s_api "${kubeconfig}" 180

    # Wait for cluster create to complete
    log_info "Waiting for cluster bootstrap to complete..."
    wait $cluster_pid || true

    # Wait for all nodes (1 CP + 2 workers = 3)
    wait_for_nodes_ready "${kubeconfig}" 3 300

    # Create workloads
    create_workloads "${kubeconfig}" "${CLUSTER_NAME}"

    log_success "Cluster ${CLUSTER_NAME} created successfully!"
}

# Create all clusters
create_all() {
    check_prerequisites

    echo ""
    echo -e "${CYAN}============================================${NC}"
    echo -e "${CYAN}  Multi-Cluster Test Environment Setup${NC}"
    echo -e "${CYAN}============================================${NC}"
    echo ""
    log_info "Creating 3 clusters with 3 nodes each (9 total nodes)"
    log_info "This will take several minutes..."
    echo ""

    local start_time=$(date +%s)

    for config in "${CLUSTERS[@]}"; do
        create_cluster "$config"
        echo ""
    done

    local elapsed=$(($(date +%s) - start_time))
    local minutes=$((elapsed / 60))
    local seconds=$((elapsed % 60))

    echo ""
    echo -e "${CYAN}============================================${NC}"
    echo -e "${GREEN}  All clusters created successfully!${NC}"
    echo -e "${CYAN}============================================${NC}"
    echo ""
    log_info "Total time: ${minutes}m ${seconds}s"
    echo ""
    echo -e "${CYAN}=== Talos Contexts ===${NC}"
    talosctl config contexts
    echo ""
    echo -e "${CYAN}=== Next Steps ===${NC}"
    echo ""
    echo "1. Start talos9s:"
    echo "   cargo run --bin talos9s"
    echo ""
    echo "2. The accordion should show all 3 clusters:"
    echo "   - cluster-alpha (3 nodes)"
    echo "   - cluster-beta (3 nodes)"
    echo "   - cluster-gamma (3 nodes)"
    echo ""
    echo "3. Use Space/Enter to expand/collapse"
    echo "4. Press 'O' for rolling operations on active cluster"
    echo ""
}

# Destroy all clusters
destroy_all() {
    echo ""
    log_info "Destroying all test clusters..."
    echo ""

    for config in "${CLUSTERS[@]}"; do
        parse_cluster_config "$config"
        log_info "Destroying ${CLUSTER_NAME}..."
        talosctl cluster destroy --name "${CLUSTER_NAME}" 2>/dev/null || log_warn "${CLUSTER_NAME} not found"
        rm -f "${OUTPUT_DIR}/${CLUSTER_NAME}-kubeconfig" 2>/dev/null || true
    done

    log_success "All clusters destroyed"
}

# Show status of all clusters
show_status() {
    echo ""
    echo -e "${CYAN}=== Multi-Cluster Status ===${NC}"
    echo ""

    for config in "${CLUSTERS[@]}"; do
        parse_cluster_config "$config"
        local kubeconfig="${OUTPUT_DIR}/${CLUSTER_NAME}-kubeconfig"

        echo -e "${MAGENTA}--- ${CLUSTER_NAME} ---${NC}"

        # Check if cluster containers exist
        if docker ps --format '{{.Names}}' | grep -q "^${CLUSTER_NAME}-"; then
            echo -e "  Docker: ${GREEN}Running${NC}"

            # Check nodes
            if [[ -f "${kubeconfig}" ]]; then
                local node_count
                node_count=$(KUBECONFIG="${kubeconfig}" kubectl get nodes --no-headers 2>/dev/null | wc -l || echo "0")
                local ready_count
                ready_count=$(KUBECONFIG="${kubeconfig}" kubectl get nodes --no-headers 2>/dev/null | grep -c " Ready " || echo "0")
                echo -e "  Nodes: ${ready_count}/${node_count} Ready"

                # Show pods in test-drainable
                local pod_count
                pod_count=$(KUBECONFIG="${kubeconfig}" kubectl get pods -n test-drainable --no-headers 2>/dev/null | wc -l || echo "0")
                echo -e "  Workloads: ${pod_count} pods in test-drainable"
            else
                echo -e "  Kubeconfig: ${YELLOW}Not found${NC}"
            fi
        else
            echo -e "  Docker: ${RED}Not running${NC}"
        fi
        echo ""
    done

    echo -e "${CYAN}=== Talos Contexts ===${NC}"
    talosctl config contexts 2>/dev/null || log_warn "Could not list contexts"
}

# Main
main() {
    local command="${1:-help}"
    shift || true

    case "${command}" in
        create)
            create_all
            ;;
        destroy)
            destroy_all
            ;;
        status)
            show_status
            ;;
        help|--help|-h)
            show_help
            ;;
        *)
            log_error "Unknown command: ${command}"
            echo "Run './multi-cluster-test.sh help' for usage"
            exit 1
            ;;
    esac
}

main "$@"

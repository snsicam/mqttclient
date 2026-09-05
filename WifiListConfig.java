package com.geeetech.app.device;

import android.annotation.SuppressLint;
import android.bluetooth.BluetoothDevice;
import android.bluetooth.BluetoothGatt;
import android.bluetooth.BluetoothGattCallback;
import android.bluetooth.BluetoothProfile;
import android.content.Context;
import android.os.Handler;
import android.os.Looper;
import android.util.Log;
import android.widget.Toast;

import com.geeetech.app.R;

import java.util.ArrayList;
import java.util.List;
import java.util.Locale;

import blufi.espressif.BlufiCallback;
import blufi.espressif.BlufiClient;
import blufi.espressif.response.BlufiScanResult;
import blufi.espressif.response.BlufiStatusResponse;
import blufi.espressif.response.BlufiVersionResponse;

/**
 * @Author : yakie@GeeeTech
 * @Email :
 * @Date : on 2025-06-24 16:19.
 * @Description :处理蓝牙返回wifi列表
 */
@SuppressLint("MissingPermission")
public class WifiListConfig {
    private static final String TAG = "WifiListConfig";
    public static final long GATT_WRITE_TIMEOUT = 10000L;
    
    private Context mContext;
    private BluetoothDevice mDevice;
    private BlufiClient mBlufiClient;
    private BluetoothGatt mBluetoothGatt;
    private boolean mConnected = false;
    private boolean mScanning = false;
    private List<GTScanResult> mWifiList;
    
    private Handler mTimeoutHandler = new Handler(Looper.getMainLooper());
    private Runnable mTimeoutRunnable;
    private boolean mHasReceivedBluetoothWifi = false;
    private static final long WIFI_LIST_TIMEOUT = 10000L; // 10秒超时
    
    private Handler mHandler = new Handler(Looper.getMainLooper());
    private boolean isReceive = false;
    private volatile boolean mWifiConfigurationSubmitted = false;
    
    /**
     * WiFi列表获取结果监听器
     */
    public interface OnWifiListResultListener {
        void onWifiListLoaded(List<GTScanResult> wifiList, boolean success);
        void onPostWifiDataSuccess();
        void onPostWifiDataFailed();
    }
    
    private OnWifiListResultListener mListener;
    
    public WifiListConfig(Context context, BluetoothDevice device) {
        this.mContext = context;
        this.mDevice = device;
        this.mWifiList = new ArrayList<>();
    }
    /**
     * 设置WiFi列表结果监听器
     */
    public void setOnWifiListResultListener(OnWifiListResultListener listener) {
        this.mListener = listener;
    }

    
    /**
     * 启动WiFi列表超时计时器
     */
    private void startWifiListTimeout() {
        mHasReceivedBluetoothWifi = false;
        // 取消之前的计时器（如果有）
        if (mTimeoutRunnable != null) {
            mTimeoutHandler.removeCallbacks(mTimeoutRunnable);
        }

        mTimeoutRunnable = () -> {
            mScanning = false;
            if (!mHasReceivedBluetoothWifi) {
                Log.d(TAG, "WiFi list timeout");
                // ✅ 取消超时计时器，避免重复调用
                cancelWifiListTimeout();
                // 超时后通知Activity
                if (mListener != null) {
                    mListener.onWifiListLoaded(mWifiList, false);
                }
            }
        };

        mTimeoutHandler.postDelayed(mTimeoutRunnable, WIFI_LIST_TIMEOUT);
        Log.d(TAG, "Timeout handler posted, will execute in " + WIFI_LIST_TIMEOUT + "ms");
    }

    /**
     * 取消超时计时器
     */
    private void cancelWifiListTimeout() {
        if (mTimeoutRunnable != null) {
            mTimeoutHandler.removeCallbacks(mTimeoutRunnable);
            mTimeoutRunnable = null;
        }
    }


    /**
     * 连接到蓝牙设备并请求 WiFi 列表。
     */
    public void connect() {
        mWifiConfigurationSubmitted = false;
        mScanning = false;
        if (mBlufiClient != null) {
            mBlufiClient.close();
            mBlufiClient = null;
        }
        // 启动10秒超时计时器
        startWifiListTimeout();
        mBlufiClient = new BlufiClient(mContext, mDevice);
        mBlufiClient.setGattCallback(new GattCallback(mBlufiClient));
        mBlufiClient.setBlufiCallback(new BlufiCallbackMain());
        mBlufiClient.setGattWriteTimeout(GATT_WRITE_TIMEOUT);
        mBlufiClient.connect();
    }

    /**
     * 在现有 BLUFI 连接上重新请求设备 WiFi 扫描。
     * 已建立的连接不应在每次刷新时断开重连，否则 ESP32 的旧 GATT 断开回调会与
     * 新连接发生竞争，表现为刷新结果交替成功、失败。
     */
    public void refreshWifiList() {
        if (mBlufiClient == null || !mConnected) {
            Log.d(TAG, "No active BLUFI connection; reconnect before WiFi scan");
            reConnect();
            return;
        }
        requestDeviceWifiScan();
    }

    // 仅供连接已经意外断开时的兜底；正常刷新使用 refreshWifiList()。
    public void reConnect() {
        close();
        mHandler.postDelayed(this::connect, 1000);
    }

    private void requestDeviceWifiScan() {
        if (mBlufiClient == null) {
            Log.w(TAG, "Cannot request WiFi scan: BLUFI client is null");
            return;
        }
        if (mScanning) {
            Log.d(TAG, "WiFi scan already in progress; ignore repeated refresh");
            return;
        }

        mScanning = true;
        startWifiListTimeout();
        Log.d(TAG, "Request device WiFi scan");
        mBlufiClient.requestDeviceWifiScan();
    }

    private void onGattConnected(BluetoothGatt gatt) {
        mBluetoothGatt = gatt;
        mConnected = true;
        Log.d(TAG, "Bluetooth connected");
        // 注意：不在这里请求WiFi列表，需要等待服务发现完成
    }

    private void onGattDisconnected(BluetoothGatt gatt) {
        // 重连时旧 GATT 的异步断开回调不能覆盖新连接的状态。
        if (mBluetoothGatt != null && mBluetoothGatt != gatt) {
            Log.d(TAG, "Ignore stale GATT disconnect callback");
            return;
        }
        mBluetoothGatt = null;
        mConnected = false;
        mScanning = false;
    }
    /**
     * 发送WiFi配置数据到设备
     */
    public void postCustumData(String dataStr) {
        if (mBlufiClient != null && !android.text.TextUtils.isEmpty(dataStr)) {
            mWifiConfigurationSubmitted = true;
            mBlufiClient.postCustomData(dataStr.getBytes());
            Log.d(TAG, "postCustumData: " + dataStr);
        }
    }


    private class GattCallback extends BluetoothGattCallback {
        private final BlufiClient mCallbackClient;

        GattCallback(BlufiClient client) {
            mCallbackClient = client;
        }

        private boolean isCurrentClient() {
            return mCallbackClient == mBlufiClient;
        }

        @Override
        public void onConnectionStateChange(BluetoothGatt gatt, int status, int newState) {
            if (!isCurrentClient()) {
                Log.d(TAG, "Ignore connection state callback from stale BLUFI client");
                gatt.close();
                return;
            }
            String devAddr = gatt.getDevice().getAddress();
            if (status == BluetoothGatt.GATT_SUCCESS) {
                switch (newState) {
                    case BluetoothProfile.STATE_CONNECTED:
                        onGattConnected(gatt);
                        Log.d(TAG,String.format("Connected %s", devAddr));
                        break;
                    case BluetoothProfile.STATE_DISCONNECTED:
                        gatt.close();
                        onGattDisconnected(gatt);
                        Log.d(TAG,String.format(String.format("Disconnected %s", devAddr)));
                        break;
                }
            } else {
                gatt.close();
                onGattDisconnected(gatt);
                Log.d(TAG,(String.format(Locale.ENGLISH, "Disconnect %s, status=%d", devAddr, status)));
            }
        }

        @Override
        public void onMtuChanged(BluetoothGatt gatt, int mtu, int status) {
            if (!isCurrentClient() || gatt != mBluetoothGatt) {
                Log.d(TAG, "Ignore MTU callback from stale GATT connection");
                return;
            }
            if (status == BluetoothGatt.GATT_SUCCESS) {
                Log.d(TAG,String.format(Locale.ENGLISH, "Set mtu complete, mtu=%d ", mtu));

                Log.d(TAG, "MTU negotiated, now requesting WiFi scan");
                requestDeviceWifiScan();
            } else {
                mBlufiClient.setPostPackageLengthLimit(20);
                Log.d(TAG,String.format(Locale.ENGLISH, "Set mtu failed, mtu=%d, status=%d", mtu, status));

                // 即使MTU协商失败，也尝试请求WiFi列表。
                Log.d(TAG, "MTU negotiation failed, still requesting WiFi scan with default MTU");
                requestDeviceWifiScan();
            }
        }
        @Override
        public void onServicesDiscovered(BluetoothGatt gatt, int status) {
            Log.d(TAG,"onServicesDiscovered status="+status);
            if (status != BluetoothGatt.GATT_SUCCESS) {
                gatt.disconnect();
                //updateMessage(String.format(Locale.ENGLISH, "Discover services error status %d", status), false);
            }
        }
    }
    private class BlufiCallbackMain extends BlufiCallback {
        @Override
        public void onGattPrepared(BlufiClient client, int status, BluetoothGatt gatt) {
            if (client != mBlufiClient || gatt != mBluetoothGatt) {
                Log.d(TAG, "Ignore prepared callback from stale BLUFI connection");
                return;
            }
            switch (status) {
                case STATUS_SUCCESS:
                    //updateMessage("Discover service and characteristics success", false);
                    Log.d(TAG,"Discover service and characteristics success");
                    
                    // ✅ 先请求MTU，等待MTU协商完成后再请求WiFi列表
                    int mtu = BlufiConstants.DEFAULT_MTU_LENGTH;
                    boolean requestMtu = gatt.requestMtu(mtu);
                    if (!requestMtu) {
                        // 部分设备拒绝 MTU 请求后不会回调 onMtuChanged，直接使用默认分包继续扫描。
                        Log.w(TAG,"Request mtu failed; scan WiFi with default MTU");
                        client.setPostPackageLengthLimit(20);
                        requestDeviceWifiScan();
                    }
                    return;
                case CODE_GATT_DISCOVER_SERVICE_FAILED:
                    Log.w(TAG,"Discover service failed");
                    gatt.disconnect();
                    //updateMessage("Discover service failed", false);
                    return;
                case CODE_GATT_DISCOVER_WRITE_CHAR_FAILED:
                    Log.w(TAG,"Get write characteristic failed");
                    gatt.disconnect();
                    //updateMessage("Get write characteristic failed", false);
                    return;
                case CODE_GATT_DISCOVER_NOTIFY_CHAR_FAILED:
                    Log.w(TAG,"Get notification characteristic failed");
                    gatt.disconnect();
                    //updateMessage("Get notification characteristic failed", false);
                    return;
                case CODE_GATT_ERR_OPEN_NOTIFY:
                    Log.w(TAG,"Open notify function failed");
                    gatt.disconnect();
                    //updateMessage("Open notify function failed", false);
                    return;
                default:
                    gatt.disconnect();
                    //updateMessage("onGattPrepared unknown status", false);
                    break;
            }
        }

        @Override
        public void onNegotiateSecurityResult(BlufiClient client, int status) {
        }

        @Override
        public void onPostConfigureParams(BlufiClient client, int status) {
        }

        @Override
        public void onDeviceStatusResponse(BlufiClient client, int status, BlufiStatusResponse response) {
            Log.d(TAG, String.format("Receive device status response, status=%d, %s", status, response.generateValidInfo()));
            if (!mWifiConfigurationSubmitted) {
                Log.d(TAG, "Ignore device status before WiFi configuration submission");
                return;
            }
            mWifiConfigurationSubmitted = false;
            if (status == STATUS_SUCCESS) {
                if (mListener != null) {
                    mListener.onPostWifiDataSuccess();
                }
            } else {
                if (mListener != null) {
                    mListener.onPostWifiDataFailed();
                }
            }
        }
        @Override
        public void onDeviceScanResult(BlufiClient client, int status, List<BlufiScanResult> results) {
            if (client != mBlufiClient) {
                Log.d(TAG, "Ignore WiFi scan result from stale BLUFI client");
                return;
            }
            mScanning = false;
            if (status == STATUS_SUCCESS) {
                mWifiList.clear();
                for (BlufiScanResult result : results) {
                    GTScanResult sr = new GTScanResult();
                    sr.ssid = result.getSsid();
                    sr.level = result.getRssi();
                    mWifiList.add(sr);
                }
                
                // 收到扫描响应即结束超时等待；空列表由 UI 作为“无可用 WiFi”处理。
                mHasReceivedBluetoothWifi = true;
                cancelWifiListTimeout();
                Log.d(TAG, "Added " + mWifiList.size() + " wifi items from bluetooth");
                
                // 通知Activity WiFi列表已加载
                if (mListener != null) {
                    mListener.onWifiListLoaded(mWifiList, true);
                }
            } else {
                Log.e(TAG, "Device scan result error, code=" + status);
                mHasReceivedBluetoothWifi = false;
                cancelWifiListTimeout();
                // 通知Activity加载失败
                if (mListener != null) {
                    mListener.onWifiListLoaded(mWifiList, false);
                }
            }
        }

        @Override
        public void onDeviceVersionResponse(BlufiClient client, int status, BlufiVersionResponse response) {
            Log.d(TAG,String.format("Receive device version response, status=%d, %s", status, response.getVersionString()));
        }
        @Override
        public void onPostCustomDataResult(BlufiClient client, int status, byte[] data) {
            if (status == STATUS_SUCCESS) {
                String customStr = new String(data);
                Log.d(TAG, String.format("Receive custom data:\n%s", customStr));
                mHandler.postDelayed(new Runnable() {
                    @Override
                    public void run() {
                        if (!isReceive) {
                            mBlufiClient.postCustomData(data);
                            Log.d(TAG, "重发数据");
                        }
                    }
                }, 2000);
            } else {
                mWifiConfigurationSubmitted = false;
                Toast.makeText(mContext, "Receive custom data error, code=" + status, Toast.LENGTH_SHORT).show();
                if (mListener != null) {
                    mListener.onPostWifiDataFailed();
                }
            }
        }

        @Override
        public void onReceiveCustomData(BlufiClient client, int status, byte[] data) {
            if (status == STATUS_SUCCESS) {
                String customStr = new String(data);
                isReceive = true;
                Log.d(TAG, String.format("Receive custom data:\n%s", customStr));
                if (customStr.contains("failed")) {
                    mWifiConfigurationSubmitted = false;
                    if (mListener != null) {
                        mListener.onPostWifiDataFailed();
                    }
                }
            } else {
                isReceive = true;
                Toast.makeText(mContext, "Receive custom data error, code=" + status, Toast.LENGTH_SHORT).show();
            }
        }

        @Override
        public void onError(BlufiClient client, int errCode) {
            if (errCode == CODE_GATT_WRITE_TIMEOUT) {
                Toast.makeText(mContext, "Timeout", Toast.LENGTH_SHORT).show();
                client.close();
                if (client == mBlufiClient) {
                    onGattDisconnected(mBluetoothGatt);
                }
            } else if (errCode == CODE_WIFI_SCAN_FAIL) {
                Toast.makeText(mContext, "Scan failed, please retry later", Toast.LENGTH_SHORT).show();
            }
        }
    }
    /**
     * 关闭连接并清理资源
     */
    public void close() {
        // ✅ 先取消超时计时器
        cancelWifiListTimeout();
        
        if (mBlufiClient != null) {
            try {
                // ✅ 添加异常保护，防止 ExecutorService 为 null
                mBlufiClient.requestCloseConnection();
            } catch (Exception e) {
                Log.w(TAG, "requestCloseConnection failed: " + e.getMessage());
            }
        }
        
        mWifiList.clear();
        mHasReceivedBluetoothWifi = false;
        mScanning = false;
        mConnected = false;
        mBluetoothGatt = null;
        mWifiConfigurationSubmitted = false;
        isReceive = false;
        
        if (mBlufiClient != null) {
            try {
                mBlufiClient.close();
            } catch (Exception e) {
                Log.w(TAG, "BlufiClient close failed: " + e.getMessage());
            }
            mBlufiClient = null;
        }
    }
    
    /**
     * 销毁并释放所有资源
     */
    public void destroy() {
        cancelWifiListTimeout();
        close();
    }
    
    /**
     * 获取WiFi列表
     */
    public List<GTScanResult> getWifiList() {
        return mWifiList;
    }
}
